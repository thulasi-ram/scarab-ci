//! Step-results ingest acceptance (ADR-0042): the trusted per-Pod egress sidecar
//! POSTs a step's named results, authenticated by a fence-scoped
//! `HMAC-SHA256(secret, "run:step:attempt")` token. A valid token persists the
//! results; a bad/missing token is 401; with no secret configured the endpoint is
//! 404; an unknown step is 404. Hermetic (InMemoryDb, no network).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{Attempt, AttemptId, AttemptOutcome, Db, RunId, StepId, StepStatus, Timestamp};
use scarab_server::{results_token_message, router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

const SECRET: &[u8] = b"results-secret";

/// `step` of `run`, Running on its first Attempt `a1` — the window in which
/// its results sidecar drains.
async fn seed_step(db: &InMemoryDb, run: &RunId, step: &StepId) {
    db.create_step_run(run, step, None, &[], Timestamp(0))
        .await
        .unwrap();
    record_running_attempt(db, run, step, "a1").await;
}

async fn record_running_attempt(db: &InMemoryDb, run: &RunId, step: &StepId, id: &str) {
    db.record_attempt(
        run,
        step,
        &Attempt {
            id: AttemptId(id.into()),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    let status = db
        .steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| &s.step == step)
        .unwrap()
        .status;
    if status != StepStatus::Running {
        db.record_step_transition(run, step, status, StepStatus::Running)
            .await
            .unwrap();
    }
}

fn token_for(attempt: &str) -> String {
    scarab_forge_github::sign_hex(
        SECRET,
        results_token_message("r1", "deploy", attempt).as_bytes(),
    )
}

fn state(db: Arc<InMemoryDb>, secret: Option<&[u8]>) -> AppState {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let mut st = AppState::new(db, Arc::new(FakeClock::new(1_000)), logs);
    if let Some(s) = secret {
        st = st.with_results_token_secret(s.to_vec());
    }
    st
}

fn ingest_req(
    run: &str,
    step: &str,
    attempt: &str,
    token: Option<&str>,
    body: &str,
) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/v1/runs/{run}/steps/{step}/results"))
        .header("content-type", "application/json")
        .header("x-scarab-attempt", attempt);
    if let Some(t) = token {
        b = b.header("x-scarab-results-token", t);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn valid_token_persists_named_results() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("deploy".into());
    seed_step(&db, &run, &step).await;
    let app = router(state(db.clone(), Some(SECRET)));

    let token = scarab_forge_github::sign_hex(
        SECRET,
        results_token_message("r1", "deploy", "a1").as_bytes(),
    );
    let body = r#"{"url":"https://svc.example","replicas":3}"#;
    let resp = app
        .oneshot(ingest_req("r1", "deploy", "a1", Some(&token), body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Typed values round-trip into step_runs.results.
    let results = db.step_results(&run, &step).await.unwrap();
    assert_eq!(
        results.get("url").unwrap(),
        &serde_json::json!("https://svc.example")
    );
    assert_eq!(
        results.get("replicas").unwrap(),
        &serde_json::json!(3),
        "int type preserved"
    );
}

#[tokio::test]
async fn bad_or_missing_token_is_unauthorized() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("deploy".into());
    seed_step(&db, &run, &step).await;
    let app = router(state(db.clone(), Some(SECRET)));

    // Wrong secret.
    let wrong = scarab_forge_github::sign_hex(
        b"nope",
        results_token_message("r1", "deploy", "a1").as_bytes(),
    );
    let resp = app
        .clone()
        .oneshot(ingest_req(
            "r1",
            "deploy",
            "a1",
            Some(&wrong),
            r#"{"url":"x"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // A token for a different attempt does not authenticate this fence.
    let other_attempt = scarab_forge_github::sign_hex(
        SECRET,
        results_token_message("r1", "deploy", "a2").as_bytes(),
    );
    let resp = app
        .clone()
        .oneshot(ingest_req(
            "r1",
            "deploy",
            "a1",
            Some(&other_attempt),
            r#"{"url":"x"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "token is fence-scoped to the attempt"
    );

    // Missing token.
    let resp = app
        .oneshot(ingest_req("r1", "deploy", "a1", None, r#"{"url":"x"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    assert!(
        db.step_results(&run, &step).await.unwrap().is_empty(),
        "nothing persisted"
    );
}

#[tokio::test]
async fn ingest_disabled_without_a_secret_is_404() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("deploy".into());
    seed_step(&db, &run, &step).await;
    let app = router(state(db, None)); // no results_token_secret

    let token = scarab_forge_github::sign_hex(
        SECRET,
        results_token_message("r1", "deploy", "a1").as_bytes(),
    );
    let resp = app
        .oneshot(ingest_req(
            "r1",
            "deploy",
            "a1",
            Some(&token),
            r#"{"url":"x"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn results_for_an_unknown_step_are_404() {
    let db = Arc::new(InMemoryDb::new());
    let app = router(state(db, Some(SECRET))); // no step seeded

    let token = scarab_forge_github::sign_hex(
        SECRET,
        results_token_message("r1", "ghost", "a1").as_bytes(),
    );
    let resp = app
        .oneshot(ingest_req(
            "r1",
            "ghost",
            "a1",
            Some(&token),
            r#"{"url":"x"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A late POST must not overwrite results the engine already consumed
/// (review safety S1/M1): the bounded drain (git-bug f02588d) can release a
/// step's verdict while its sidecar is still retrying, and dependents then
/// read the results as they stood. Once the step has settled — or the token's
/// attempt is no longer the step's frontier — the ingest refuses with 409 and
/// writes nothing.
#[tokio::test]
async fn results_for_a_settled_or_superseded_attempt_are_refused() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("deploy".into());
    seed_step(&db, &run, &step).await;
    let app = router(state(db.clone(), Some(SECRET)));

    // a1 is superseded by a retry a2 (step back through Pending, Running a2).
    db.record_step_transition(&run, &step, StepStatus::Running, StepStatus::Pending)
        .await
        .unwrap();
    record_running_attempt(&db, &run, &step, "a2").await;
    let resp = app
        .clone()
        .oneshot(ingest_req(
            "r1",
            "deploy",
            "a1",
            Some(&token_for("a1")),
            r#"{"v":"old"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "a1 is not the frontier"
    );
    assert!(db.step_results(&run, &step).await.unwrap().is_empty());

    // The frontier's own drain lands while the step runs.
    let resp = app
        .clone()
        .oneshot(ingest_req(
            "r1",
            "deploy",
            "a2",
            Some(&token_for("a2")),
            r#"{"v":"a2"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // The step settles; a late retry of the same POST is refused.
    db.record_step_transition(&run, &step, StepStatus::Running, StepStatus::Succeeded)
        .await
        .unwrap();
    let resp = app
        .oneshot(ingest_req(
            "r1",
            "deploy",
            "a2",
            Some(&token_for("a2")),
            r#"{"v":"late"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "the step already settled"
    );
    assert_eq!(
        db.step_results(&run, &step).await.unwrap().get("v"),
        Some(&serde_json::json!("a2")),
        "the consumed results are not overwritten"
    );
}
