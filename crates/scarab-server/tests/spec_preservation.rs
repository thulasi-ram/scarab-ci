//! API→StepSpec field preservation (test-strategy Phase 1).
//!
//! A real escaped bug was `create_run` silently dropping `run_as_root` on the
//! way from the API payload to the durable `StepSpec` — the run "worked" but the
//! Pod launched without the requested grant. This suite pins the whole
//! translation, table-driven: POST /v1/runs with every field the inline API
//! supports, drive one scheduler tick, and assert the spec the executor
//! actually received carries each field. Hermetic — real router + real engine
//! over InMemoryDb + FakeExecutor (ADR-0017).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use scarab_engine::{Clock, Db, RunId, Scheduler, StepId};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryObjectStore};

fn app(db: Arc<InMemoryDb>, clock: Arc<FakeClock>) -> axum::Router {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    router(AppState::new(db, clock, logs))
}

async fn post_run(app: &axum::Router, body: serde_json::Value) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Every security/placement/runtime field the inline `POST /v1/runs` payload
/// supports survives the API→StepSpec translation and reaches the executor's
/// `launch` verbatim. Table-driven so a regression names the dropped field.
#[tokio::test]
async fn create_run_preserves_every_step_spec_field_to_the_executor() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(FakeExecutor::new());
    let app = app(db.clone(), clock.clone());

    let resp = post_run(
        &app,
        json!({
            "pipeline": {
                "ir_version": 1,
                "steps": [{
                    "id": "work",
                    "image": "ci/build:1.2.3",
                    "command": ["make", "test"],
                    "env": { "FOO": "bar", "MODE": "ci" },
                    "secrets": ["db-pass", "api-key"],
                    // Self-service grant (ADR-0039) — the field a real escaped
                    // bug dropped in create_run.
                    "security": { "run_as_root": true },
                    "timeout": 900,
                    "placement_profiles": ["gpu", "spot"],
                    "resources": { "cpu_millis": 1500, "memory_mib": 2048 },
                    // Sidecar service (ADR-0058) co-located in the step's Pod.
                    "services": [{
                        "image": "postgres:16",
                        "args": ["-c", "fsync=off"],
                        "env": { "POSTGRES_PASSWORD": "test" },
                        "ports": [5432],
                        "ready": { "tcp": 5432 },
                        "run_as_user": 999
                    }]
                }]
            }
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = body_json(resp).await["id"].as_str().unwrap().to_string();
    let run = RunId(id);

    // One scheduler tick: admit → claim → launch via the FakeExecutor.
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    sched.tick_all().await.unwrap();

    let step = db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == StepId("work".into()))
        .expect("step exists");
    let handle = FakeExecutor::handle_for(&step);
    let spec = exec
        .launched_spec(&handle)
        .expect("the step was launched — its spec was handed to the executor");

    let expected_service = scarab_pipeline::ServiceSpec {
        image: "postgres:16".into(),
        command: vec![],
        args: vec!["-c".into(), "fsync=off".into()],
        env: BTreeMap::from([("POSTGRES_PASSWORD".to_string(), "test".to_string())]),
        ports: vec![5432],
        ready: Some(scarab_pipeline::ReadyProbe {
            tcp: Some(5432),
            exec: vec![],
            http: None,
        }),
        run_as_user: Some(999),
        run_as_root: false,
    };

    // The table: (field, preserved?, what the executor actually saw).
    let checks: Vec<(&str, bool, String)> = vec![
        (
            "image",
            spec.image == "ci/build:1.2.3",
            format!("{:?}", spec.image),
        ),
        (
            "command",
            spec.command == vec!["make".to_string(), "test".to_string()],
            format!("{:?}", spec.command),
        ),
        (
            "env",
            spec.env
                == vec![
                    ("FOO".to_string(), "bar".to_string()),
                    ("MODE".to_string(), "ci".to_string()),
                ],
            format!("{:?}", spec.env),
        ),
        (
            "secrets",
            spec.secrets == vec!["db-pass".to_string(), "api-key".to_string()],
            format!("{:?}", spec.secrets),
        ),
        // The escaped-bug field: security.run_as_root is self-service on an
        // inline run and MUST survive to the executor.
        (
            "run_as_root",
            spec.run_as_root,
            format!("{:?}", spec.run_as_root),
        ),
        (
            "timeout_seconds",
            spec.timeout_seconds == Some(900),
            format!("{:?}", spec.timeout_seconds),
        ),
        (
            "placement_profiles",
            spec.placement_profiles == vec!["gpu".to_string(), "spot".to_string()],
            format!("{:?}", spec.placement_profiles),
        ),
        (
            "resources.cpu_millis",
            spec.resources.cpu_millis == Some(1500),
            format!("{:?}", spec.resources.cpu_millis),
        ),
        (
            "resources.memory_mib",
            spec.resources.memory_mib == Some(2048),
            format!("{:?}", spec.resources.memory_mib),
        ),
        (
            "services (sidecars)",
            spec.services == vec![expected_service],
            format!("{:?}", spec.services),
        ),
    ];

    let dropped: Vec<String> = checks
        .iter()
        .filter(|(_, ok, _)| !ok)
        .map(|(field, _, got)| format!("  {field}: executor saw {got}"))
        .collect();
    assert!(
        dropped.is_empty(),
        "field(s) dropped/mangled between POST /v1/runs and executor launch:\n{}",
        dropped.join("\n")
    );
}

/// `uses:` (shared-service opt-in, ADR-0058) survives to the DURABLE spec. The
/// step deliberately never launches here — the scheduler holds an opt-in step
/// Pending until its named service is ready, and an inline run declares no
/// pipeline-level services — so the assertion is on the stored StepSpec (the
/// same spec every launch derives from), not on a launch.
#[tokio::test]
async fn create_run_preserves_uses_on_the_durable_spec() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let app = app(db.clone(), clock.clone());

    let resp = post_run(
        &app,
        json!({
            "pipeline": {
                "ir_version": 1,
                "steps": [{
                    "id": "test",
                    "image": "busybox",
                    "command": ["true"],
                    "uses": ["cache"]
                }]
            }
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let spec = db
        .step_spec(&RunId(id), &StepId("test".into()))
        .await
        .unwrap()
        .expect("durable spec stored");
    assert_eq!(spec.uses, vec!["cache".to_string()]);
}

/// Fail-closed pins (ADR-0039, ADR-0055): an inline API run targets no
/// Environment, so governed grants and a raw k8s overlay must be rejected with
/// a 400 — and reject BEFORE any run exists (no half-created run).
#[tokio::test]
async fn inline_governed_grants_and_overlay_are_rejected_fail_closed() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let app = app(db.clone(), clock.clone());

    let step_with = |extra: serde_json::Value| {
        let mut step = json!({ "id": "work", "image": "busybox", "command": ["true"] });
        step.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({ "pipeline": { "ir_version": 1, "steps": [step] } })
    };

    for (name, payload) in [
        (
            "privileged",
            step_with(json!({ "security": { "privileged": true } })),
        ),
        (
            "add_capabilities",
            step_with(json!({ "security": { "add_capabilities": ["NET_ADMIN"] } })),
        ),
        (
            "k8s_overlay",
            step_with(json!({ "k8s_overlay": { "spec": { "hostNetwork": true } } })),
        ),
    ] {
        let resp = post_run(&app, payload).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "`{name}` on an inline run must be rejected fail-closed"
        );
    }
}

/// A rejected `POST /v1/runs` must create **no** run — the 4xx contract. Pinned
/// with a two-step pipeline whose SECOND step is rejected: nothing of the
/// request may persist, or the scheduler would execute the accepted first step
/// of a request whose caller was told failed outright.
#[tokio::test]
async fn rejected_create_run_persists_nothing() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let app = app(db.clone(), clock.clone());

    let resp = post_run(
        &app,
        json!({
            "pipeline": {
                "ir_version": 1,
                "steps": [
                    { "id": "ok", "image": "busybox", "command": ["true"] },
                    {
                        "id": "bad",
                        "image": "busybox",
                        "command": ["true"],
                        "security": { "privileged": true }
                    }
                ]
            }
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let runs = db.list_runs(100).await.unwrap();
    assert!(
        runs.is_empty(),
        "a rejected create_run must persist NOTHING — found {} run(s), whose \
         already-persisted steps the scheduler would happily execute",
        runs.len()
    );
}
