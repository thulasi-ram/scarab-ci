//! Feature acceptance (ADR-0017): explicit `inputs:` workspace selection reaches
//! the executor as the actual materialized workspace set.
//!
//! ADR-0007/0029: a step inherits every `needs` output workspace by default, but
//! an authored `inputs:` subset restricts what flows in. This drives the FEATURE
//! through the real router + real scheduler over InMemoryDb + FakeExecutor: a
//! diamond `{B, C} -> D` where D declares `inputs: [B]` and a sibling E inherits
//! both, then asserts on the `workspace_inputs` each step's spec carried into the
//! executor's `launch`. The failure mode (D materializing C's workspace) is
//! FakeExecutor-observable, so no kind-tier case is needed at land time.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, RunId, Scheduler, StepId, StepRun};
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

/// The workspace inputs the executor was handed for a launched step, by step id.
fn launched_workspace_inputs(exec: &FakeExecutor, steps: &[StepRun], id: &str) -> Vec<String> {
    let step = steps
        .iter()
        .find(|s| s.step == StepId(id.into()))
        .unwrap_or_else(|| panic!("step {id} exists"));
    let handle = FakeExecutor::handle_for(step);
    exec.launched_spec(&handle)
        .unwrap_or_else(|| panic!("step {id} was launched — its spec reached the executor"))
        .workspace_inputs
}

/// The declared output paths the executor was handed for a launched step.
fn launched_workspace_outputs(exec: &FakeExecutor, steps: &[StepRun], id: &str) -> Vec<String> {
    let step = steps
        .iter()
        .find(|s| s.step == StepId(id.into()))
        .unwrap_or_else(|| panic!("step {id} exists"));
    exec.launched_spec(&FakeExecutor::handle_for(step))
        .unwrap_or_else(|| panic!("step {id} was launched — its spec reached the executor"))
        .workspace_outputs
}

/// The sibling half of ADR-0007: an authored `outputs:` must reach the executor,
/// which is what lets the egress leg prune the published snapshot to those paths.
/// This pins the *plumbing* (pipeline → IR → spec → launch) — the pruning itself
/// is proven in `scarab-storage-s3/tests/workspace.rs`, and the Pod-annotation
/// hop in `scarab-executor-k8s`'s `declared_outputs_ride_the_pod_annotation`.
/// Before this landed, `outputs:` was parsed, validated, and then dropped.
#[tokio::test]
async fn explicit_outputs_reach_the_executor_spec() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(FakeExecutor::new());
    let app = app(db.clone(), clock.clone());

    let resp = post_run(
        &app,
        json!({
            "pipeline": {
                "ir_version": 1,
                "steps": [
                    {
                        "id": "build",
                        "image": "busybox",
                        "command": ["true"],
                        // The feature under test: publish only these paths.
                        "outputs": ["dist", "reports/junit/results.xml"]
                    },
                    {
                        // Control: no `outputs:` → publish the whole workspace.
                        "id": "test",
                        "image": "busybox",
                        "command": ["true"]
                    }
                ]
            }
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let run = RunId(body_json(resp).await["id"].as_str().unwrap().to_string());

    for _ in 0..8 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    for _ in 0..4 {
        sched.tick_all().await.unwrap();
    }

    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(
        launched_workspace_outputs(&exec, &steps, "build"),
        vec!["dist".to_string(), "reports/junit/results.xml".to_string()],
        "the authored `outputs:` must survive compile + persistence and reach launch"
    );
    assert!(
        launched_workspace_outputs(&exec, &steps, "test").is_empty(),
        "no `outputs:` must stay empty — the whole-workspace default is unchanged"
    );
}

/// Diamond `{B, C} -> {D, E}`. B and C each publish a distinct output workspace.
/// D declares `inputs: [B]`; E declares no `inputs:` (implicit-by-default).
/// After driving the scheduler, D's launched workspace must be exactly `[B]`
/// (C excluded), and E's must be both `[B, C]`.
#[tokio::test]
async fn explicit_inputs_scopes_the_materialized_workspace_at_launch() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(FakeExecutor::new());
    let app = app(db.clone(), clock.clone());

    let resp = post_run(
        &app,
        json!({
            "pipeline": {
                "ir_version": 1,
                "steps": [
                    { "id": "b", "image": "busybox", "command": ["true"] },
                    { "id": "c", "image": "busybox", "command": ["true"] },
                    {
                        "id": "d",
                        "image": "busybox",
                        "command": ["true"],
                        "needs": ["b", "c"],
                        // The feature under test: consume only B's workspace.
                        "inputs": ["b"]
                    },
                    {
                        // Control: no `inputs:` → inherit BOTH needs (back-compat).
                        "id": "e",
                        "image": "busybox",
                        "command": ["true"],
                        "needs": ["b", "c"]
                    }
                ]
            }
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let run = RunId(body_json(resp).await["id"].as_str().unwrap().to_string());

    // B and C each publish a distinct output workspace snapshot on success, so
    // the workspace set D vs. E receives is observably different.
    exec.set_output("b", "snap-b");
    exec.set_output("c", "snap-c");
    // Every poll reports success; the diamond needs a few ticks to drain
    // (B/C succeed, then D/E become ready and launch).
    for _ in 0..16 {
        exec.script_outcome(ExecState::Succeeded);
    }

    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    for _ in 0..8 {
        sched.tick_all().await.unwrap();
    }

    let steps = db.steps_of_run(&run).await.unwrap();

    // D declared `inputs: [b]`: it materializes ONLY B's workspace — C excluded.
    assert_eq!(
        launched_workspace_inputs(&exec, &steps, "d"),
        vec!["snap-b".to_string()],
        "D declares `inputs: [b]` so only B's output workspace flows in; C's must be excluded"
    );

    // E declared no `inputs:`: implicit-by-default inherits BOTH needs' outputs,
    // in `needs` order.
    assert_eq!(
        launched_workspace_inputs(&exec, &steps, "e"),
        vec!["snap-b".to_string(), "snap-c".to_string()],
        "E declares no `inputs:` so it inherits every need's workspace (back-compat)"
    );
}

/// The order leg of the fan-in diamond (ticket 2e1a458): merge order comes
/// from the PINNED IR's declared `needs:` order — not alphabetical, not
/// completion order. Nothing pinned a reversal until this test: `needs:
/// ["c", "b"]` must hand the executor `[snap-c, snap-b]`, so the last-wins
/// winner is B — the declared list read backwards would flip who wins.
#[tokio::test]
async fn declared_needs_order_reaches_launch_even_reversed() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(FakeExecutor::new());
    let app = app(db.clone(), clock.clone());

    let resp = post_run(
        &app,
        json!({
            "pipeline": {
                "ir_version": 1,
                "steps": [
                    { "id": "b", "image": "busybox", "command": ["true"] },
                    { "id": "c", "image": "busybox", "command": ["true"] },
                    {
                        // The feature under test: needs declared REVERSED
                        // relative to both definition order and the alphabet.
                        "id": "d",
                        "image": "busybox",
                        "command": ["true"],
                        "needs": ["c", "b"]
                    }
                ]
            }
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let run = RunId(body_json(resp).await["id"].as_str().unwrap().to_string());

    exec.set_output("b", "snap-b");
    exec.set_output("c", "snap-c");
    for _ in 0..12 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    for _ in 0..6 {
        sched.tick_all().await.unwrap();
    }

    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(
        launched_workspace_inputs(&exec, &steps, "d"),
        vec!["snap-c".to_string(), "snap-b".to_string()],
        "the DECLARED `needs:` order is the merge order (ADR-0007) — \
         reversing the declaration must reverse the roots"
    );
}

/// The event leg of the fan-in diagnostic (ticket 2e1a458): a backend-reported
/// `ProvisioningReport` becomes an `EventPayload::WorkspaceInputCollisions`
/// at settle, with the report's ROOT indices resolved to step ids through the
/// same consumed-and-produced-output filter `workspace_inputs` applies. The
/// fixture makes the filter observable: `a` is declared first but produces NO
/// output, so root index 0 is B and index 1 is C — a mapping through the raw
/// needs list would blame `a`.
#[tokio::test]
async fn a_reported_collision_becomes_an_event_with_step_ids_resolved() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(FakeExecutor::new());
    let app = app(db.clone(), clock.clone());

    let resp = post_run(
        &app,
        json!({
            "pipeline": {
                "ir_version": 1,
                "steps": [
                    { "id": "a", "image": "busybox", "command": ["true"] },
                    { "id": "b", "image": "busybox", "command": ["true"] },
                    { "id": "c", "image": "busybox", "command": ["true"] },
                    {
                        "id": "e",
                        "image": "busybox",
                        "command": ["true"],
                        "needs": ["a", "b", "c"]
                    }
                ]
            }
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let run = RunId(body_json(resp).await["id"].as_str().unwrap().to_string());

    // `a` deliberately produces NO output snapshot: it must not occupy a root
    // index. B and C do; the backend reports one collision where C (root 1)
    // overwrote B (root 0).
    exec.set_output("b", "snap-b");
    exec.set_output("c", "snap-c");
    exec.set_provisioning(
        "e",
        scarab_engine::ProvisioningReport {
            collisions: 1,
            sample: vec![scarab_engine::ProvisioningCollision {
                path: "shared.txt".into(),
                winner: 1,
                loser: 0,
            }],
        },
    );
    for _ in 0..16 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    for _ in 0..8 {
        sched.tick_all().await.unwrap();
    }

    let events = db.events(&run).await.unwrap();
    let collision = events
        .iter()
        .find_map(|e| match &e.kind {
            scarab_engine::EventPayload::WorkspaceInputCollisions {
                step,
                count,
                sample,
                ..
            } => Some((step.clone(), *count, sample.clone())),
            _ => None,
        })
        .expect("a WorkspaceInputCollisions event lands at settle");
    let (step, count, sample) = collision;
    assert_eq!(step, StepId("e".into()));
    assert_eq!(count, 1);
    assert_eq!(sample.len(), 1);
    assert_eq!(sample[0].path, "shared.txt");
    assert_eq!(
        (sample[0].winner.clone(), sample[0].loser.clone()),
        (StepId("c".into()), StepId("b".into())),
        "indices resolve through the produced-output filter: root 0 is B \
         (a produced nothing), root 1 is C — never a/b off the raw needs list"
    );
    // Steps that reported no collisions appended no event.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e.kind,
                scarab_engine::EventPayload::WorkspaceInputCollisions { .. }
            ))
            .count(),
        1,
        "only the colliding step's settle appends the event"
    );
}
