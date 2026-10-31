//! Postgres cannot store U+0000 in `text` or `jsonb`, so a NUL reaching a
//! write failed it with a 500 (git-bug 10584e2). Each path that takes text
//! from outside: a caller's dispatch reason is refused with a 400; a forge's
//! commit subject or pull request title, and a Step's results, are stored
//! with the NUL removed or replaced. PG-backed: the in-memory store accepts a
//! NUL, so only Postgres shows the failure.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use scarab_db_postgres::PostgresDb;
use scarab_engine::{Attempt, AttemptId, AttemptOutcome, Db, RunId, StepId, StepStatus, Timestamp};
use scarab_forge::{Event, RepoRef};
use scarab_server::{
    results_token_message, router, trigger_runs_from_event, AdmissionPorts, AppState, LogService,
};
use scarab_testkit::{FakeClock, FakeForge, InMemoryObjectStore};

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

async fn pg() -> Option<(common::TestDb, Arc<PostgresDb>)> {
    let tdb = common::fresh_db().await?;
    let db = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    db.migrate().await.unwrap();
    Some((tdb, db))
}

fn state(db: Arc<PostgresDb>) -> AppState {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
}

async fn post(app: &axum::Router, uri: &str, body: serde_json::Value) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// A dispatch reason, from the dispatch endpoint or an inline `POST /v1/runs`,
/// holding a NUL is a 400, and no Run is created.
#[tokio::test]
async fn a_nul_in_a_dispatch_reason_is_a_400() {
    let Some((tdb, db)) = pg().await else {
        return;
    };
    let forge = Arc::new(
        FakeForge::new()
            .with_file(
                ".scarab/ship.yaml",
                "on:\n  manual: {}\nsteps:\n  - { id: ship, image: busybox, command: [\"true\"] }\n",
            )
            .with_commit("refs/heads/main", SHA),
    );
    let app = router(state(db.clone()).with_forge(forge));

    let (status, body) = post(
        &app,
        "/v1/repos/acme/web/dispatch",
        json!({ "ref": "refs/heads/main", "pipeline": "ship", "reason": "ship\u{0} it" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("NUL"), "{body}");

    let (status, body) = post(
        &app,
        "/v1/runs",
        json!({
            "pipeline": {
                "ir_version": 1,
                "steps": [{ "id": "build", "image": "busybox:latest", "command": ["true"] }]
            },
            "reason": "ship\u{0} it"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    assert!(db.list_runs(10).await.unwrap().is_empty());
    tdb.cleanup().await;
}

/// A commit subject (or pull request title) holding a NUL is stored as the
/// Run's Headline without it: the forge's text is not the Run's to refuse.
#[tokio::test]
async fn a_nul_in_a_commit_subject_is_stripped_from_the_headline() {
    let Some((tdb, db)) = pg().await else {
        return;
    };
    let forge = FakeForge::new()
        .with_file(
            ".scarab/ci.yaml",
            "on:\n  push: {}\nsteps:\n  - { id: test, image: busybox, command: [\"true\"] }\n",
        )
        .with_commit("refs/heads/main", SHA);
    let push = Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "fix\u{0} the build\n\nbody".into(),
    };
    let out = trigger_runs_from_event(
        &forge,
        db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::default(),
        &push,
    )
    .await
    .expect("the Run is created");
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    assert_eq!(
        db.run_trigger_title(&out.runs[0]).await.unwrap().as_deref(),
        Some("fix the build")
    );
    tdb.cleanup().await;
}

/// A Step's results holding a NUL, in a value or a name, are stored with each
/// NUL replaced by U+FFFD: the results arrive, and the reader can see where
/// the text was changed.
#[tokio::test]
async fn a_nul_in_step_results_is_replaced_not_a_500() {
    let Some((tdb, db)) = pg().await else {
        return;
    };
    let run = RunId("r1".into());
    let step = StepId("deploy".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &step, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.record_attempt(
        &run,
        &step,
        &Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    db.record_step_transition(&run, &step, StepStatus::Pending, StepStatus::Running)
        .await
        .unwrap();
    const SECRET: &[u8] = b"results-secret";
    let app = router(state(db.clone()).with_results_token_secret(SECRET.to_vec()));
    let token = scarab_forge_github::sign_hex(
        SECRET,
        results_token_message("r1", "deploy", "a1").as_bytes(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs/r1/steps/deploy/results")
                .header("content-type", "application/json")
                .header("x-scarab-attempt", "a1")
                .header("x-scarab-results-token", token)
                .body(Body::from(
                    json!({ "url": "a\u{0}b", "list": ["c\u{0}"], "k\u{0}": 1 }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let results = db.step_results(&run, &step).await.unwrap();
    assert_eq!(results.get("url"), Some(&json!("a\u{FFFD}b")));
    assert_eq!(results.get("list"), Some(&json!(["c\u{FFFD}"])));
    assert_eq!(results.get("k\u{FFFD}"), Some(&json!(1)));
    tdb.cleanup().await;
}
