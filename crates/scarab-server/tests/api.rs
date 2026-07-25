//! REST API acceptance (ADR-0012): drive the dogfooded surface in-process with
//! a real engine over InMemoryDb + a fake executor. Proves the happy path
//! (POST run → scheduler → GET Succeeded), that logs stream as SSE, and that
//! OpenAPI is served. No Postgres/cluster needed — hermetic.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt; // oneshot

use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, RunId, Scheduler};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryObjectStore};

/// Build an AppState over the given in-memory store and a fresh clock.
fn app_state(db: Arc<InMemoryDb>, clock: Arc<FakeClock>) -> AppState {
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    AppState::new(db, clock, logs)
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    serde_json::from_str(&body_string(resp).await).unwrap()
}

#[tokio::test]
async fn happy_path_post_run_then_scheduler_reaches_succeeded() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let app = router(app_state(db.clone(), clock.clone()));

    // POST /v1/runs with an inline 1-step pipeline (the IR subset).
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
    let created = body_json(resp).await;
    assert_eq!(created["status"], "pending");
    let id = created["id"].as_str().unwrap().to_string();

    // Drive the run to completion with the real scheduler + a fake executor.
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);
    {
        let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &exec, "sched-1");
        sched.tick(&RunId(id.clone())).await.unwrap();
    }

    // GET /v1/runs/:id -> Succeeded.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let status = body_json(resp).await;
    assert_eq!(status["status"], "succeeded");
    assert_eq!(status["steps"][0]["id"], "build");
    assert_eq!(status["steps"][0]["status"], "succeeded");

    // GET /v1/runs/:id/events -> SSE tail of the event log.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let sse = body_string(resp).await;
    assert!(
        sse.contains("RunCreated"),
        "SSE should carry the event log: {sse}"
    );
    assert!(sse.contains("RunTransitioned"));
}

#[tokio::test]
async fn list_runs_returns_recent_runs_newest_first() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let app = router(app_state(db.clone(), clock.clone()));

    // Create two runs with distinct creation times (advance the clock between).
    db.create_run(
        &RunId("older".into()),
        1,
        1,
        scarab_engine::Timestamp(1_000),
    )
    .await
    .unwrap();
    db.create_run(
        &RunId("newer".into()),
        1,
        1,
        scarab_engine::Timestamp(2_000),
    )
    .await
    .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = body_json(resp).await;
    let runs = doc["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    // Newest first.
    assert_eq!(runs[0]["id"], "newer");
    assert_eq!(runs[0]["status"], "pending");
    assert_eq!(runs[0]["created_at"], 2_000);
    assert_eq!(runs[1]["id"], "older");

    // `limit` caps the page.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/runs?limit=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let doc = body_json(resp).await;
    let runs = doc["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "limit=1 returns one run");
    assert_eq!(runs[0]["id"], "newer");
}

#[tokio::test]
async fn openapi_is_served_and_describes_the_ir_subset() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(0));
    let app = router(app_state(db, clock));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = body_json(resp).await;
    assert!(doc["paths"]["/v1/runs"]["post"].is_object());
    // The request schema is the IR subset: pipeline -> steps -> {image, command}.
    let schemas = &doc["components"]["schemas"];
    assert!(schemas["StepDto"]["properties"]["image"].is_object());
    assert!(schemas["PipelineDto"]["properties"]["ir_version"].is_object());
}

#[tokio::test]
async fn unknown_run_is_404() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(0));
    let app = router(app_state(db, clock));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/runs/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_run_exposes_step_needs_for_the_dag() {
    // The run detail view renders a DAG, so GET /v1/runs/:id must surface each
    // step's `needs` in-edges (ADR-0006), not just id/status/attempts.
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let app = router(app_state(db, clock));

    let body = serde_json::json!({
        "pipeline": {
            "ir_version": 1,
            "steps": [
                { "id": "build", "image": "busybox:latest", "command": ["echo", "b"] },
                { "id": "test", "image": "busybox:latest", "command": ["echo", "t"], "needs": ["build"] }
            ]
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
    let id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let status = body_json(resp).await;
    let steps = status["steps"].as_array().unwrap();
    let test = steps
        .iter()
        .find(|s| s["id"] == "test")
        .expect("test step present");
    assert_eq!(
        test["needs"],
        serde_json::json!(["build"]),
        "DAG in-edges surfaced"
    );
    let build = steps.iter().find(|s| s["id"] == "build").unwrap();
    assert_eq!(
        build["needs"],
        serde_json::json!([]),
        "root step has no needs"
    );
}

/// Full-route OpenAPI coverage (ADR-0054): every route registered on the
/// router appears in the generated spec (and vice versa), so the committed
/// openapi.json — which CI diffs against — can never silently under-describe
/// the API. Parses the router source. Two exemptions: `/openapi.json` (the
/// spec serving itself) and the deprecated `.../steps/{step}/restart` alias —
/// the pre-rename name for `.../rerun` (2026-07-23), kept live for old callers
/// but intentionally off the documented surface.
#[test]
fn every_registered_route_is_in_the_openapi_spec() {
    let src = include_str!("../src/lib.rs");
    let re = regex_lite::Regex::new(r#"\.route\(\s*"([^"]+)""#).unwrap();
    let mut routes: Vec<String> = re
        .captures_iter(src)
        .map(|c| c[1].replace("{*name}", "{name}"))
        .filter(|r| r != "/openapi.json")
        .filter(|r| r != "/v1/runs/{id}/steps/{step}/restart")
        .collect();
    routes.sort();
    routes.dedup();

    let spec: serde_json::Value = serde_json::from_str(&scarab_server::openapi_json()).unwrap();
    let have: std::collections::BTreeSet<String> =
        spec["paths"].as_object().unwrap().keys().cloned().collect();

    let missing: Vec<&String> = routes.iter().filter(|r| !have.contains(*r)).collect();
    assert!(
        missing.is_empty(),
        "routes missing from the OpenAPI spec: {missing:?}"
    );
    let extra: Vec<&String> = have.iter().filter(|p| !routes.contains(p)).collect();
    assert!(
        extra.is_empty(),
        "spec paths with no registered route: {extra:?}"
    );
}

/// Every operation carries exactly one curated group, and every group it names
/// is declared top-level (git-bug 305ce8a). The doc renderer builds a page per
/// operation from tags, so an untagged operation silently vanishes from the
/// published reference — this is the gate that makes a new route pick a group.
#[test]
fn operation_tags_cover_every_operation() {
    const METHODS: [&str; 7] = ["get", "put", "post", "delete", "options", "head", "patch"];

    let spec: serde_json::Value = serde_json::from_str(&scarab_server::openapi_json()).unwrap();
    let declared: std::collections::BTreeSet<&str> = spec["tags"]
        .as_array()
        .expect("spec declares top-level tags")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();

    let mut untagged = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        for method in METHODS {
            let Some(op) = item.get(method) else { continue };
            match op["tags"].as_array().map(Vec::as_slice) {
                Some([tag]) => {
                    let tag = tag.as_str().unwrap();
                    assert!(
                        declared.contains(tag),
                        "{method} {path} is tagged {tag:?}, which is not declared top-level"
                    );
                    used.insert(tag.to_owned());
                }
                _ => untagged.push(format!("{method} {path}")),
            }
        }
    }
    untagged.sort();
    assert!(
        untagged.is_empty(),
        "operations with no group — extend `tag_for_path`: {untagged:?}"
    );
    // No empty sidebar sections: a declared group must have members.
    let unused: Vec<&&str> = declared.iter().filter(|t| !used.contains(**t)).collect();
    assert!(unused.is_empty(), "declared groups with no operations: {unused:?}");
}

/// The embedded web UI (ADR-0054): `/` serves index.html, real assets serve
/// with their content type, an SPA client route falls back to index, and the
/// API keeps winning under /v1. Path traversal cannot escape the dist dir.
#[tokio::test]
async fn embedded_ui_serves_index_assets_and_spa_fallback() {
    let dist = tempfile::tempdir().unwrap();
    std::fs::write(dist.path().join("index.html"), "<html>scarab-ui</html>").unwrap();
    std::fs::create_dir(dist.path().join("assets")).unwrap();
    std::fs::write(dist.path().join("assets/app.js"), "console.log(1)").unwrap();

    let db = std::sync::Arc::new(scarab_testkit::InMemoryDb::new());
    let store = std::sync::Arc::new(scarab_testkit::InMemoryObjectStore::new());
    let logs = std::sync::Arc::new(scarab_server::LogService::new(store, db.clone()));
    let app = scarab_server::router(
        scarab_server::AppState::new(
            db,
            std::sync::Arc::new(scarab_testkit::FakeClock::new(0)),
            logs,
        )
        .with_ui_dir(dist.path()),
    );

    let get = |uri: &str| {
        axum::http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let body = |resp: axum::response::Response| async {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    };

    // Index at /.
    let resp = app.clone().oneshot(get("/")).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert!(String::from_utf8_lossy(&body(resp).await).contains("scarab-ui"));

    // A real asset with its content type.
    let resp = app.clone().oneshot(get("/assets/app.js")).await.unwrap();
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/javascript")
    );

    // An SPA client route falls back to index (client routing takes over).
    let resp = app
        .clone()
        .oneshot(get("/acme/web/runs/r-123"))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert!(String::from_utf8_lossy(&body(resp).await).contains("scarab-ui"));

    // The API still wins under /v1 (JSON, not HTML).
    let resp = app.clone().oneshot(get("/v1/runs")).await.unwrap();
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );

    // Traversal cannot escape dist: `..` segments are dropped, so this is
    // the SPA fallback, never /etc/passwd.
    let resp = app
        .clone()
        .oneshot(get("/../../../../etc/passwd"))
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body(resp).await).contains("scarab-ui"));
}

/// Shared-service evidence (ADR-0058 slice 4): a run's Services panel reads
/// `GET /v1/runs/{id}/services` for the current Take's instances + their
/// lifecycle status, and `GET …/services/{name}/logs` replays that instance's
/// best-effort log tail through the SAME pipeline as step logs (ADR-0013).
#[tokio::test]
async fn shared_service_status_and_logs_are_exposed() {
    use scarab_engine::{RunStatus, ServiceStatus, Timestamp};

    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    // Build state by hand so the test appends to the SAME LogService the app reads.
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    let app = router(AppState::new(db.clone(), clock, logs.clone()));

    let run = RunId("run-with-svc".into());
    // Terminal so the log SSE replays and closes (hermetic — no live tail hang).
    db.seed_run(&run, RunStatus::Succeeded);
    // A shared service instance at Take 1, promoted to `ready`.
    db.create_run_service(&run, 1, "db", Timestamp(1_000))
        .await
        .unwrap();
    db.set_run_service(
        &run,
        1,
        "db",
        ServiceStatus::Ready,
        Some("scarab-svc-db-abcd"),
    )
    .await
    .unwrap();
    // Some best-effort log output on that instance's stream.
    let (step, attempt) = scarab_server::logs::service_stream_key("db", 1);
    logs.append(
        &run,
        &step,
        &attempt,
        b"postgres is ready to accept connections\n",
    )
    .await
    .unwrap();

    // Status list: the current Take's `db`, `ready`.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/run-with-svc/services")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = body_json(resp).await;
    let svcs = doc.as_array().unwrap();
    assert_eq!(svcs.len(), 1);
    assert_eq!(svcs[0]["name"], "db");
    assert_eq!(svcs[0]["status"], "ready");
    assert_eq!(svcs[0]["take"], 1);

    // Logs: the appended bytes replay over SSE.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/run-with-svc/services/db/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp)
        .await
        .contains("postgres is ready to accept connections"));

    // An unknown service on a known run is 404.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/runs/run-with-svc/services/nope/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
