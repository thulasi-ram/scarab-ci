//! Step ids that are not one plain path segment, through the real router
//! with the web UI mounted (as the chart runs it).
//!
//! An invoked library's Steps are `{invoke id}/{id}` (`amd64/image`), and a
//! matrix Step's id is `base[k=v,…]`. A URL carries such an id as one
//! percent-encoded segment and `Path` decodes it back, so results ingest and
//! the results read work for it, and the results token signs the raw id. A
//! URL that carries the raw `/` matches no API route, and an unknown `/v1`
//! path answers a JSON 404 for every method: before, the web UI fallback
//! answered it 200 with index.html, and the results sidecar took that for a
//! stored result.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{Attempt, AttemptId, AttemptOutcome, Db, RunId, StepId, StepStatus, Timestamp};
use scarab_pipeline::url_path_segment;
use scarab_server::{results_token_message, router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

const SECRET: &[u8] = b"results-secret";
const STEP_IDS: [&str; 2] = ["amd64/image", "test[os=linux,arch=arm64]"];

/// The router over `db`, with results ingest on and a web UI in `dist`.
fn app(db: Arc<InMemoryDb>, dist: &std::path::Path) -> axum::Router {
    std::fs::write(dist.join("index.html"), "<!doctype html>scarab-ui").unwrap();
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    router(
        AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
            .with_results_token_secret(SECRET.to_vec())
            .with_ui_dir(dist),
    )
}

/// Run `r1` with each of [`STEP_IDS`] Running on attempt `a1`.
async fn seed(db: &InMemoryDb) {
    let run = RunId("r1".into());
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
        let status = db
            .steps_of_run(&run)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step == step)
            .unwrap()
            .status;
        if status != StepStatus::Running {
            db.record_step_transition(&run, &step, status, StepStatus::Running)
                .await
                .unwrap();
        }
    }
}

fn ingest(path: &str, step_id: &str, body: &str) -> Request<Body> {
    let token = scarab_forge_github::sign_hex(
        SECRET,
        results_token_message("r1", step_id, "a1").as_bytes(),
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

fn content_type(resp: &axum::response::Response) -> Option<String> {
    resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

#[tokio::test]
async fn results_of_an_invoked_or_matrix_step_round_trip_through_the_router() {
    let db = Arc::new(InMemoryDb::new());
    seed(&db).await;
    let dist = tempfile::tempdir().unwrap();
    let app = app(db.clone(), dist.path());

    for id in STEP_IDS {
        let path = format!("/v1/runs/r1/steps/{}/results", url_path_segment(id));
        let resp = app
            .clone()
            .oneshot(ingest(&path, id, r#"{"image":{"digest":"sha256:ab"}}"#))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED, "{id}: {path}");

        // Stored under the raw id.
        let stored = db
            .step_results(&RunId("r1".into()), &StepId(id.into()))
            .await
            .unwrap();
        assert_eq!(
            stored.get("image"),
            Some(&serde_json::json!({"digest": "sha256:ab"})),
            "{id}"
        );

        // And read back through the same encoded path.
        let resp = app.clone().oneshot(get(&path)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{id}");
        assert_eq!(content_type(&resp).as_deref(), Some("application/json"));
        let body: serde_json::Value = serde_json::from_str(&text(resp).await).unwrap();
        assert_eq!(body[0]["name"], "image", "{id}: {body}");
        assert_eq!(body[0]["value"]["digest"], "sha256:ab", "{id}: {body}");
    }
}

#[tokio::test]
async fn a_raw_slash_in_a_step_id_is_a_json_404_not_the_web_ui() {
    let db = Arc::new(InMemoryDb::new());
    seed(&db).await;
    let dist = tempfile::tempdir().unwrap();
    let app = app(db.clone(), dist.path());

    let raw = "/v1/runs/r1/steps/amd64/image/results";
    let resp = app
        .clone()
        .oneshot(ingest(raw, "amd64/image", r#"{"image":1}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(content_type(&resp).as_deref(), Some("application/json"));
    assert!(!text(resp).await.contains("scarab-ui"));
    assert!(db
        .step_results(&RunId("r1".into()), &StepId("amd64/image".into()))
        .await
        .unwrap()
        .is_empty());

    let resp = app.clone().oneshot(get(raw)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(content_type(&resp).as_deref(), Some("application/json"));
}

#[tokio::test]
async fn an_unknown_api_path_is_a_json_404_for_every_method() {
    let db = Arc::new(InMemoryDb::new());
    let dist = tempfile::tempdir().unwrap();
    let app = app(db, dist.path());

    for method in ["GET", "POST", "PUT", "DELETE", "HEAD"] {
        for path in ["/v1/no-such-route", "/v1/runs/r1/no/such/route", "/v1"] {
            let req = Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(if method == "POST" { "{}" } else { "" }))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method} {path}");
            assert_eq!(
                content_type(&resp).as_deref(),
                Some("application/json"),
                "{method} {path}"
            );
            if method != "HEAD" {
                let body: serde_json::Value = serde_json::from_str(&text(resp).await).unwrap();
                assert_eq!(body["code"], "not_found", "{method} {path}");
            }
        }
    }

    // A web UI route still gets index.html.
    let resp = app
        .clone()
        .oneshot(get("/acme/web/runs/r-123"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(text(resp).await.contains("scarab-ui"));
}
