//! Step keys through the real router, with the web UI mounted (as the chart
//! runs it).
//!
//! The run detail gives each step a key, and every step route takes it in
//! place of the authored id: results ingest (whose token still signs the
//! authored id), the results read and the raw log. An authored id,
//! percent-encoded, still works. A key that names no step of the run (an
//! unknown key, another run's key) is a 404, never the web UI.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{Attempt, AttemptId, AttemptOutcome, Db, RunId, StepId, StepStatus, Timestamp};
use scarab_pipeline::url_path_segment;
use scarab_server::{results_token_message, router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

const SECRET: &[u8] = b"results-secret";
const STEP_IDS: [&str; 3] = ["build", "amd64/image", "test[os=linux,arch=arm64]"];

/// The router over `db` and `logs`, with results ingest on and a web UI in
/// `dist`.
fn app(db: Arc<InMemoryDb>, logs: Arc<LogService>, dist: &std::path::Path) -> axum::Router {
    std::fs::write(dist.join("index.html"), "<!doctype html>scarab-ui").unwrap();
    router(
        AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
            .with_results_token_secret(SECRET.to_vec())
            .with_ui_dir(dist),
    )
}

/// Run `run` with each of [`STEP_IDS`] Running on attempt `a1`, each with
/// one line of log.
async fn seed(db: &InMemoryDb, logs: &LogService, run: &str) {
    let run = RunId(run.into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    for id in STEP_IDS {
        let step = StepId(id.into());
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
        logs.append(
            &run,
            &step,
            &AttemptId("a1".into()),
            format!("log of {id}\n").as_bytes(),
        )
        .await
        .unwrap();
    }
}

/// The key of each step of `run`, as the run detail lists it.
async fn keys(app: &axum::Router, run: &str) -> std::collections::BTreeMap<String, String> {
    let resp = app
        .clone()
        .oneshot(get(&format!("/v1/runs/{run}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&text(resp).await).unwrap();
    body["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["id"].as_str().unwrap().to_string(),
                s["key"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// A results POST to `path`, its token signing `signed` (`run:step:a1`).
fn ingest(path: &str, signed: (&str, &str), body: &str) -> Request<Body> {
    let token = scarab_forge_github::sign_hex(
        SECRET,
        results_token_message(signed.0, signed.1, "a1").as_bytes(),
    );
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("x-scarab-attempt", "a1")
        .header("x-scarab-results-token", token)
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

async fn text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn setup() -> (
    axum::Router,
    Arc<InMemoryDb>,
    std::collections::BTreeMap<String, String>,
    tempfile::TempDir,
) {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    seed(&db, &logs, "r1").await;
    seed(&db, &logs, "r2").await;
    let dist = tempfile::tempdir().unwrap();
    let app = app(db.clone(), logs, dist.path());
    let keys = keys(&app, "r1").await;
    (app, db, keys, dist)
}

#[tokio::test]
async fn the_run_detail_gives_every_step_a_distinct_key() {
    let (app, _db, keys, _dist) = setup().await;
    assert_eq!(keys.len(), STEP_IDS.len());
    for (id, key) in &keys {
        assert!(scarab_engine::is_step_key_shape(key), "{id}: {key}");
    }
    let distinct: std::collections::BTreeSet<_> = keys.values().collect();
    assert_eq!(distinct.len(), keys.len());
    // Another run's steps have their own keys.
    let other = self::keys(&app, "r2").await;
    assert!(other.values().all(|k| !distinct.contains(k)));
}

#[tokio::test]
async fn results_and_logs_go_through_a_step_key() {
    let (app, db, keys, _dist) = setup().await;
    for id in STEP_IDS {
        let key = &keys[id];
        let path = format!("/v1/runs/r1/steps/{key}/results");
        // The token signs the authored id; the URL carries the key.
        let resp = app
            .clone()
            .oneshot(ingest(&path, ("r1", id), r#"{"digest":"sha256:ab"}"#))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED, "{id}: {path}");
        let stored = db
            .step_results(&RunId("r1".into()), &StepId(id.into()))
            .await
            .unwrap();
        assert_eq!(stored.get("digest"), Some(&serde_json::json!("sha256:ab")));

        let resp = app.clone().oneshot(get(&path)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{id}");
        let body: serde_json::Value = serde_json::from_str(&text(resp).await).unwrap();
        assert_eq!(body[0]["name"], "digest", "{id}: {body}");

        let resp = app
            .clone()
            .oneshot(get(&format!("/v1/runs/r1/steps/{key}/logs/raw?attempt=a1")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{id}");
        assert!(text(resp).await.contains(&format!("log of {id}")), "{id}");

        // The authored id, percent-encoded, names the same step.
        let encoded = format!("/v1/runs/r1/steps/{}/results", url_path_segment(id));
        let resp = app.clone().oneshot(get(&encoded)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{id}: {encoded}");
        let body: serde_json::Value = serde_json::from_str(&text(resp).await).unwrap();
        assert_eq!(body[0]["name"], "digest", "{id}: {body}");
    }
}

#[tokio::test]
async fn a_results_token_must_sign_the_step_the_key_names() {
    let (app, db, keys, _dist) = setup().await;
    let path = format!("/v1/runs/r1/steps/{}/results", keys["amd64/image"]);
    // Signed for another step of the run, or over the key itself: refused.
    for signed in [("r1", "build"), ("r1", keys["amd64/image"].as_str())] {
        let resp = app
            .clone()
            .oneshot(ingest(&path, signed, r#"{"digest":"x"}"#))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{signed:?}");
    }
    assert!(db
        .step_results(&RunId("r1".into()), &StepId("amd64/image".into()))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_key_that_names_no_step_of_the_run_is_a_404() {
    let (app, _db, _keys, _dist) = setup().await;
    let other = keys(&app, "r2").await;
    let unknown = "zzzzzzzzzzzz".to_string();
    for key in [&unknown, &other["build"]] {
        for path in [
            format!("/v1/runs/r1/steps/{key}/results"),
            format!("/v1/runs/r1/steps/{key}/logs/raw?attempt=a1"),
        ] {
            let resp = app.clone().oneshot(get(&path)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
            assert!(!text(resp).await.contains("scarab-ui"), "{path}");
        }
    }
    // An ingest to an unknown key has no step whose id its token can sign.
    let resp = app
        .clone()
        .oneshot(ingest(
            &format!("/v1/runs/r1/steps/{unknown}/results"),
            ("r1", "build"),
            r#"{"digest":"x"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A run whose `build` step succeeded and whose `approve` gate waits, driven
/// by the real scheduler, and the keys its run detail lists.
async fn gated_run() -> (
    axum::Router,
    Arc<InMemoryDb>,
    RunId,
    std::collections::BTreeMap<String, String>,
    Arc<FakeClock>,
    Arc<scarab_testkit::FakeExecutor>,
) {
    use scarab_engine::ports::ExecState;
    use scarab_engine::{Clock, RunStatus, Scheduler};
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(scarab_testkit::FakeExecutor::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db.clone(), clock.clone(), logs));
    // Seeded as the other gate tests seed one (run_gate_view.rs): `build`,
    // then a manual gate `approve` that needs it.
    let run = RunId("rg".into());
    let now = clock.now().await;
    let spec: scarab_engine::StepSpec = serde_json::from_value(
        serde_json::json!({ "image": "busybox", "command": ["true"], "env": [] }),
    )
    .unwrap();
    let build = StepId("build".into());
    let approve = StepId("approve".into());
    db.create_run(&run, 2, 1, now).await.unwrap();
    db.create_step_run(&run, &build, Some(&spec), &[], now)
        .await
        .unwrap();
    db.create_step_run(&run, &approve, None, std::slice::from_ref(&build), now)
        .await
        .unwrap();
    db.set_step_gate(&run, &approve, "manual", None)
        .await
        .unwrap();
    exec.script_outcome(ExecState::Succeeded);
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    for _ in 0..5 {
        sched.tick_all().await.unwrap();
    }
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Suspended),
        "precondition: the gate waits"
    );
    let keys = keys(&app, &run.0).await;
    (app, db, run, keys, clock, exec)
}

fn post(path: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap()
}

#[tokio::test]
async fn a_gate_is_approved_and_a_step_rerun_through_their_keys() {
    use scarab_engine::{Clock, RunStatus, Scheduler};
    let (app, db, run, keys, clock, exec) = gated_run().await;

    let resp = app
        .clone()
        .oneshot(post(&format!(
            "/v1/runs/{}/gates/{}/approve",
            run.0, keys["approve"]
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body: serde_json::Value = serde_json::from_str(&text(resp).await).unwrap();
    assert_eq!(body["released"], true, "{body}");
    let approved = db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .any(|e| matches!(e.kind, scarab_engine::EventPayload::GateApproved { .. }));
    assert!(
        approved,
        "the approval is recorded on the gate the key names"
    );
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    for _ in 0..5 {
        sched.tick_all().await.unwrap();
    }
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded),
        "the released gate let the run finish"
    );

    let resp = app
        .clone()
        .oneshot(post(&format!(
            "/v1/runs/{}/steps/{}/rerun",
            run.0, keys["build"]
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let target = db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .find_map(|e| match e.kind {
            scarab_engine::EventPayload::RunRerunRequested { target, .. } => Some(target),
            _ => None,
        });
    assert_eq!(target, Some(StepId("build".into())));
    // The rerun keeps every key.
    assert_eq!(self::keys(&app, &run.0).await, keys);
}

/// A key-shaped `{step}` that names no step of the run (here, the gate's key
/// in another server's run) approves nothing.
#[tokio::test]
async fn an_unknown_key_approves_nothing() {
    let (app, db, run, _keys, _clock, _exec) = gated_run().await;
    let (_, _, _, elsewhere, _, _) = gated_run().await;
    let resp = app
        .clone()
        .oneshot(post(&format!(
            "/v1/runs/{}/gates/{}/approve",
            run.0, elsewhere["approve"]
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let approved = db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .any(|e| matches!(e.kind, scarab_engine::EventPayload::GateApproved { .. }));
    assert!(!approved);
}
