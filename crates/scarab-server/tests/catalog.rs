//! Pipeline dispatch catalog + interface describe endpoints (ADR-0043 slice 3),
//! end-to-end over the HTTP router with in-memory fakes — hermetic, no Postgres.
//!
//! Covers: the catalog lists `.scarab/*.{yaml,yml}` with correct `manual`/`api`
//! flags and the resolved SHA, excludes `.scarab/lib/**`, tolerates an absent
//! `.scarab/` (empty list) and a single unparseable file (per-file error, rest
//! still list); the interface describe returns the compiled typed `ParamSpec`s
//! (an optional param with a default, a choice with options) + SHA + opt-in
//! flags; a bad name is a 404 and a compile error a structured 4xx (never 500).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::Db;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

// A manual+api dispatchable pipeline declaring a typed interface: a required
// choice with options, and an optional number with a default.
const DEPLOY_YAML: &str = r#"
on:
  manual: {}
  api: {}
interface:
  inputs:
    - { name: region, type: choice, options: [us-east-1, eu-west-1], description: "target region" }
    - { name: replicas, type: number, required: false, default: 2 }
steps:
  - id: ship
    image: busybox
    command: ["deploy", "${{ inputs.region }}"]
"#;

// A push-only pipeline (not dispatchable) — appears in the catalog with both
// flags false.
const CI_YAML: &str = "on: { push: {} }\nsteps: [{ id: build, image: busybox }]";

fn app(forge: scarab_testkit::FakeForge) -> axum::Router {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()));
    let forge: Arc<dyn scarab_forge::ForgePort> = Arc::new(forge);
    router(AppState::new(db as Arc<dyn Db>, clock, logs).with_forge(forge))
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn catalog_lists_pipelines_with_flags_and_resolved_sha_excluding_lib() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_file(".scarab/ci.yml", CI_YAML)
        // A library under .scarab/lib/** must NOT appear in the catalog.
        .with_file(".scarab/lib/build.yaml", "steps: [{ id: b, image: busybox }]")
        .with_commit("refs/heads/main", "sha-abc123");

    let app = app(forge);
    let (status, body) =
        get_json(&app, "/v1/repos/acme/web/pipelines?ref=refs/heads/main").await;
    assert_eq!(status, StatusCode::OK);

    // The ref resolved to the seeded concrete commit.
    assert_eq!(body["sha"], "sha-abc123");

    let pipelines = body["pipelines"].as_array().unwrap();
    let names: Vec<&str> = pipelines.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["ci", "deploy"], "path-sorted bare names, lib excluded");

    let deploy = pipelines.iter().find(|p| p["name"] == "deploy").unwrap();
    assert_eq!(deploy["manual"], true);
    assert_eq!(deploy["api"], true);

    let ci = pipelines.iter().find(|p| p["name"] == "ci").unwrap();
    assert_eq!(ci["manual"], false, "push-only pipeline is not manually dispatchable");
    assert_eq!(ci["api"], false);
}

#[tokio::test]
async fn catalog_of_absent_scarab_dir_is_empty_not_an_error() {
    // No `.scarab/*` files seeded → list_dir_at_ref yields nothing.
    let forge = scarab_testkit::FakeForge::new().with_commit("HEAD", "sha-empty");
    let app = app(forge);
    let (status, body) = get_json(&app, "/v1/repos/acme/web/pipelines").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sha"], "sha-empty");
    assert!(body["pipelines"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn catalog_flags_a_single_unparseable_file_without_failing_the_list() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        // Malformed YAML — `on:` cannot parse.
        .with_file(".scarab/broken.yaml", "on: [this is not a map");
    let app = app(forge);
    let (status, body) = get_json(&app, "/v1/repos/acme/web/pipelines").await;
    assert_eq!(status, StatusCode::OK);

    let pipelines = body["pipelines"].as_array().unwrap();
    assert_eq!(pipelines.len(), 2, "both files listed; the broken one is flagged");
    let broken = pipelines.iter().find(|p| p["name"] == "broken").unwrap();
    assert!(broken["error"].as_str().is_some(), "broken file carries an error");
    let deploy = pipelines.iter().find(|p| p["name"] == "deploy").unwrap();
    assert!(deploy["error"].is_null(), "the good sibling still lists cleanly");
}

#[tokio::test]
async fn interface_returns_typed_specs_and_resolved_sha() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_commit("refs/heads/main", "sha-deadbeef");
    let app = app(forge);

    let (status, body) =
        get_json(&app, "/v1/repos/acme/web/pipelines/deploy/interface?ref=refs/heads/main").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sha"], "sha-deadbeef");
    assert_eq!(body["manual"], true);
    assert_eq!(body["api"], true);

    let inputs = body["inputs"].as_array().unwrap();
    assert_eq!(inputs.len(), 2);

    let region = inputs.iter().find(|p| p["name"] == "region").unwrap();
    assert_eq!(region["type"], "choice");
    assert_eq!(region["required"], true);
    assert_eq!(region["options"], serde_json::json!(["us-east-1", "eu-west-1"]));
    assert_eq!(region["description"], "target region");

    let replicas = inputs.iter().find(|p| p["name"] == "replicas").unwrap();
    assert_eq!(replicas["type"], "number");
    assert_eq!(replicas["required"], false);
    assert_eq!(replicas["default"], serde_json::json!(2), "optional param carries its default");
}

#[tokio::test]
async fn interface_of_unknown_pipeline_is_404() {
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/deploy.yaml", DEPLOY_YAML);
    let app = app(forge);
    let (status, _) = get_json(&app, "/v1/repos/acme/web/pipelines/nope/interface").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn interface_of_a_pipeline_that_fails_to_compile_is_a_structured_4xx() {
    // A duplicate step id fails validation at compile — a 4xx diagnostic, not 500.
    let bad = "on: { manual: {} }\nsteps:\n  - { id: a, image: busybox }\n  - { id: a, image: busybox }\n";
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/bad.yaml", bad);
    let app = app(forge);
    let (status, _) = get_json(&app, "/v1/repos/acme/web/pipelines/bad/interface").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "compile error is a fail-closed 4xx");
}
