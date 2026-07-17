//! Typed launch parameters (ADR-0043) end-to-end over in-memory fakes.
//!
//! Two surfaces:
//!   * the **engine** binding — a run's frozen params interpolate `${{ inputs.x }}`
//!     into a launched step, a numeric guard compares numerically (not
//!     lexicographically), unreferenced params still reach the step as
//!     `SCARAB_PARAM_*` env, and a re-launched attempt re-derives byte-identical
//!     interpolation (restart determinism);
//!   * the **supply path** — `POST /v1/runs` resolves supplied params against the
//!     declared interface, rejecting a missing required param *before* creating a
//!     run and persisting the resolved blob on a valid one.
//!
//! No Postgres/cluster — hermetic.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use scarab_engine::ports::{ExecHandle, ExecState, Executor};
use scarab_engine::{
    Clock, Db, RunId, Scheduler, StepId, StepRun, StepSpec, Timestamp,
};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

/// A recorded launch: its (already-interpolated) command and env.
type Launch = (Vec<String>, Vec<(String, String)>);

/// An executor that records the command + env of every launch, and reports each
/// launch as an immediate success.
#[derive(Default)]
struct RecordingExec {
    launches: Mutex<Vec<Launch>>,
}
#[async_trait]
impl Executor for RecordingExec {
    async fn launch(&self, _step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        self.launches
            .lock()
            .unwrap()
            .push((spec.command.clone(), spec.env.clone()));
        Ok(ExecHandle("h".into()))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        Ok(ExecState::Succeeded)
    }
    async fn cancel(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
}
use scarab_engine::ExecError;

fn interp_spec() -> StepSpec {
    StepSpec {
        image: "busybox".into(),
        // `region` is a string param; `inputs.n > 80` is a *numeric* guard — it
        // only evaluates because `n` is stored as a JSON number (a string would
        // make CEL error comparing string > number, failing the step).
        command: vec![
            "deploy".into(),
            "${{ inputs.region }}".into(),
            "big=${{ inputs.n > 80 }}".into(),
        ],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        clone: None,
    }
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn params_interpolate_inputs_and_restart_re_derives_identically() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let run = RunId("r1".into());
    let step = StepId("deploy".into());

    // A run with frozen, already-resolved (typed) launch params. `zone` is
    // *unreferenced* — it must still reach the step as env.
    db.create_run(&run, 2, 1, Timestamp(1_000)).await.unwrap();
    let params = std::collections::BTreeMap::from([
        ("region".to_string(), json!("us-east-1")),
        ("n".to_string(), json!(90)),
        ("zone".to_string(), json!("a")),
    ]);
    db.set_run_params(&run, &params).await.unwrap();
    db.create_step_run(&run, &step, Some(&interp_spec()), &[], Timestamp(1_000))
        .await
        .unwrap();

    let exec = Arc::new(RecordingExec::default());

    // First launch.
    {
        let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched-1");
        sched.tick(&run).await.unwrap();
    }

    // Restart the step (a new Attempt) and drive again.
    scarab_engine::restart_step(&*db as &dyn Db, &*clock as &dyn Clock, &run, &step)
        .await
        .unwrap();
    {
        let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched-1");
        sched.tick(&run).await.unwrap();
    }

    let launches = exec.launches.lock().unwrap().clone();
    assert_eq!(launches.len(), 2, "launched once, then again after restart");

    // `${{ inputs.region }}` interpolated; the numeric guard resolved numerically.
    let (cmd, env) = &launches[0];
    assert_eq!(cmd, &vec!["deploy".to_string(), "us-east-1".to_string(), "big=true".to_string()]);

    // Every param — including the unreferenced `zone` — reaches the step as env.
    assert!(env.contains(&("SCARAB_PARAM_REGION".into(), "us-east-1".into())), "{env:?}");
    assert!(env.contains(&("SCARAB_PARAM_N".into(), "90".into())), "{env:?}");
    assert!(env.contains(&("SCARAB_PARAM_ZONE".into(), "a".into())), "{env:?}");

    // Restart determinism (ADR-0027): the re-launched attempt re-derives the
    // exact same interpolation + env from the frozen params.
    assert_eq!(launches[0], launches[1], "re-launch is byte-identical");
}

// --- supply path (POST /v1/runs) ---------------------------------------------

fn app(db: Arc<InMemoryDb>, clock: Arc<FakeClock>) -> axum::Router {
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()));
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

#[tokio::test]
async fn post_run_with_valid_params_persists_the_resolved_blob() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let app = app(db.clone(), clock);

    let resp = post_run(
        &app,
        json!({
            "pipeline": {
                "ir_version": 2,
                "interface": {
                    "inputs": [
                        { "name": "region", "type": "string" },
                        { "name": "replicas", "type": "number", "required": false, "default": 2 }
                    ]
                },
                "steps": [{ "id": "build", "image": "busybox", "command": ["echo", "${{ inputs.region }}"] }]
            },
            "params": { "region": "us-east-1", "replicas": "5" }
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = body_json(resp).await["id"].as_str().unwrap().to_string();

    // The resolved (coerced, defaults-aware) params are frozen on the run.
    let stored = db.run_params(&RunId(id)).await.unwrap();
    assert_eq!(stored["region"], json!("us-east-1"));
    assert_eq!(stored["replicas"], json!(5), "string '5' coerced to number");
}

#[tokio::test]
async fn post_run_missing_required_param_is_rejected_and_creates_no_run() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let app = app(db.clone(), clock);

    let resp = post_run(
        &app,
        json!({
            "pipeline": {
                "ir_version": 2,
                "interface": { "inputs": [{ "name": "region", "type": "string" }] },
                "steps": [{ "id": "build", "image": "busybox" }]
            },
            "params": {}
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // No run was created (rejected pre-persist).
    let runs = db.list_runs(100).await.unwrap();
    assert!(runs.is_empty(), "a rejected launch creates no run");
}
