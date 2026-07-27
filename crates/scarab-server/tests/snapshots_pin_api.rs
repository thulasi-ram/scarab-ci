//! ADR-0061 s5 over the REST surface: the cold tier's **time bound stated
//! explicitly** on the run resource, the manual **pin** that holds it open, and
//! the **rerun plan** preview a client reads before pressing a rerun.
//!
//! Hermetic — real router + real engine over `InMemoryDb`, fakes only at the
//! ports (ADR-0017 "functional" tier). No Postgres, no cluster, no CAS: the
//! store-presence half is proven against a REAL sweeper and a REAL CAS in
//! `retention.rs`; what is under test here is the API contract, i.e. that a
//! client can find out the promise and change it.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt; // oneshot

use scarab_engine::{Db, EventPayload, RunId, RunStatus, StepId, Timestamp};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn state(db: Arc<InMemoryDb>, clock: Arc<FakeClock>) -> AppState {
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()));
    // 7 days, deliberately NOT the default: the API must quote the deployment's
    // window, not a constant baked into a DTO.
    AppState::new(db, clock, logs).with_snapshot_retention_days(7)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// A settled run with a `clone → test` chain, created at t=0.
async fn seed(db: &InMemoryDb, run: &RunId, terminal: bool) {
    db.create_run(run, 1, 1, Timestamp(0)).await.unwrap();
    db.record_transition(run, RunStatus::Pending, RunStatus::Running)
        .await
        .unwrap();
    db.record_transition(
        run,
        RunStatus::Running,
        if terminal {
            RunStatus::Succeeded
        } else {
            RunStatus::Suspended
        },
    )
    .await
    .unwrap();
    db.create_step_run(run, &StepId("clone".into()), None, &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        run,
        &StepId("test".into()),
        None,
        &[StepId("clone".into())],
        Timestamp(0),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn run_detail_states_the_cold_tier_time_bound() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    // Two days after the run settled: inside a 7-day window.
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(2 * DAY_MS));
    let run = RunId("r-terminal".into());
    seed(&db, &run, true).await;
    let app = router(state(db.clone(), clock.clone()));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r-terminal")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let wr = body_json(resp).await["snapshot_retention"].clone();
    assert_eq!(
        wr["retention_days"], 7,
        "the API quotes the deployment's window, not a hardcoded default"
    );
    assert_eq!(
        wr["expires_at"],
        7 * DAY_MS,
        "the promise is settled_at + window"
    );
    // NOT a test of *which* column the promise is keyed on. `InMemoryDb` reports
    // creation time (see its `run_snapshot_retention`) while the real adapter
    // reads `updated_at`, and here the two are the same number — so this
    // assertion would pass against either. That claim needs Postgres, and it has
    // it: `retention.rs::the_retention_promise_is_keyed_on_updated_at_not_created_at`
    // forces the two columns a hundred days apart.
    assert_eq!(wr["expired"], false);
    assert_eq!(wr["pinned"], false);
}

#[tokio::test]
async fn a_non_terminal_run_has_no_expiry_at_all() {
    // ADR-0050: a run that has not settled — including one suspended on a gate for
    // weeks — is never GC-eligible regardless of age. Reporting an expiry date for
    // it would be a lie the sweeper does not tell.
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(900 * DAY_MS));
    let run = RunId("r-suspended".into());
    seed(&db, &run, false).await;
    let app = router(state(db.clone(), clock.clone()));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r-suspended")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let wr = body_json(resp).await["snapshot_retention"].clone();
    assert!(
        wr.get("expires_at").is_none(),
        "no expiry is promised for a run that has not settled, at any age"
    );
    assert_eq!(wr["expired"], false);
}

#[tokio::test]
async fn an_aged_terminal_run_reports_expired() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(30 * DAY_MS));
    let run = RunId("r-old".into());
    seed(&db, &run, true).await;
    let app = router(state(db.clone(), clock.clone()));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r-old")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let wr = body_json(resp).await["snapshot_retention"].clone();
    assert_eq!(
        wr["expired"], true,
        "past the window the PROMISE has lapsed — which is not the same claim as 'deleted'"
    );
}

#[tokio::test]
async fn pin_then_unpin_round_trips_and_is_audited() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(2 * DAY_MS));
    let run = RunId("r-pin".into());
    seed(&db, &run, true).await;
    let app = router(state(db.clone(), clock.clone()));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs/r-pin/snapshots-pin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let wr = body_json(resp).await;
    assert_eq!(wr["pinned"], true);
    assert_eq!(
        wr["pinned_at"], 2 * DAY_MS,
        "the pin records WHEN — the audit half"
    );
    assert!(
        wr.get("expires_at").is_none(),
        "a pinned run has no expiry: the pin holds the cold tier's window open"
    );

    // Visible on the run resource too, so a client that already polls the run
    // needs no extra request to render the state.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r-pin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["snapshot_retention"]["pinned"], true);

    // The durable fact carries into the mark set: a pinned run's roots are
    // reachable no matter how old the cutoff is.
    assert!(db
        .run_snapshot_retention(&run)
        .await
        .unwrap()
        .unwrap()
        .pinned_at
        .is_some());

    // ...and the pin/unpin pair is in the event log.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/runs/r-pin/snapshots-pin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["pinned"], false);

    let kinds: Vec<String> = db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::RunSnapshotsPinned { .. } => Some("pinned".to_string()),
            EventPayload::RunSnapshotsUnpinned { .. } => Some("unpinned".to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["pinned", "unpinned"],
        "both directions are attributable — keeping data and releasing it both cost something"
    );
}

#[tokio::test]
async fn pinning_an_unknown_run_is_404() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(0));
    let app = router(state(db, clock));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs/ghost/snapshots-pin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rerun_plan_previews_the_scope_before_the_button_is_pressed() {
    // No CAS is wired here, so there is no snapshot oracle and nothing can widen —
    // the preview is the plain invalidation set. That is the contract the UI reads
    // in every deployment that has no workspace store, and it must be a real
    // answer rather than a 404 or an empty object.
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(0));
    let run = RunId("r-plan".into());
    seed(&db, &run, true).await;
    let app = router(state(db.clone(), clock.clone()));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r-plan/steps/clone/rerun-plan")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let plan = body_json(resp).await;
    assert_eq!(plan["target"], "clone");
    assert_eq!(
        plan["invalidated"],
        serde_json::json!(["clone", "test"]),
        "rerunning clone cascades to its descendants (ADR-0027)"
    );
    assert_eq!(plan["widened"], serde_json::json!([]));
    assert_eq!(plan["starts_from"], serde_json::json!(["clone"]));
    assert_eq!(plan["expired_inputs"], serde_json::json!([]));

    // An unknown step is a 404, not an empty plan — a preview of nothing would
    // read as "this rerun does nothing", which is worse than an error.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r-plan/steps/ghost/rerun-plan")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The widened plan over the wire, with **non-empty** `widened` and
/// `expired_inputs`.
///
/// The test above pins the empty case, which every field of the projection
/// satisfies vacuously: `widened: []` and `expired_inputs: []` are what a DTO
/// that dropped the fields entirely, or that swapped `consumer` and
/// `produced_by`, would also produce. And the mapping is what the affordance
/// reads out loud — "*test* will re-run too, because *build*'s workspace
/// expired". Naming the wrong step there is a plausible bug that no engine-level
/// test can catch, because the engine's `ExpiredInput` is correct: the risk lives
/// entirely in the projection.
#[tokio::test]
async fn a_widened_rerun_plan_names_the_consumer_and_the_producer_over_the_wire() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(0));
    let run = RunId("r-widen".into());

    // A `clone → build → test` chain. A real CAS, not a stub: the whole question
    // is whether a snapshot is actually absent, and only a store can answer that.
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.record_transition(&run, RunStatus::Pending, RunStatus::Running)
        .await
        .unwrap();
    db.record_transition(&run, RunStatus::Running, RunStatus::Succeeded)
        .await
        .unwrap();
    let mut prev: Option<StepId> = None;
    for name in ["clone", "build", "test"] {
        let step = StepId(name.to_string());
        let needs: Vec<StepId> = prev.clone().into_iter().collect();
        db.create_step_run(&run, &step, None, &needs, Timestamp(0))
            .await
            .unwrap();
        prev = Some(step);
    }

    let cas_dir = tempfile::tempdir().unwrap();
    let cas: Arc<dyn scarab_storage::Cas> = Arc::new(
        scarab_storage_s3::S3Storage::local(cas_dir.path().to_str().unwrap()).unwrap(),
    );
    // `clone`'s snapshot is really there; `build`'s really is not. So rerunning
    // `test` has to walk back exactly one step — far enough to be non-empty,
    // short enough that "the whole chain" would be a visibly different answer.
    let live = {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("src.txt"), b"cloned").unwrap();
        cas.ingest(dir.path().to_str().unwrap()).await.unwrap().root.0
    };
    const SWEPT: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    db.set_step_output(
        &run,
        &StepId("clone".into()),
        &scarab_engine::AttemptId("a1".into()),
        &live,
        None,
    )
    .await
    .unwrap();
    db.set_step_output(
        &run,
        &StepId("build".into()),
        &scarab_engine::AttemptId("a1".into()),
        SWEPT,
        None,
    )
    .await
    .unwrap();

    let app = router(state(db.clone(), clock.clone()).with_workspace_cas(cas));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r-widen/steps/test/rerun-plan")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let plan = body_json(resp).await;

    assert_eq!(plan["target"], "test");
    assert_eq!(
        plan["invalidated"],
        serde_json::json!(["build", "test"]),
        "the target plus the ancestor that regenerates its input"
    );
    assert_eq!(
        plan["widened"],
        serde_json::json!(["build"]),
        "the widened subset is the ancestor alone — it must NOT include the target,          or the copy would say a plain rerun was widened"
    );
    assert_eq!(
        plan["starts_from"],
        serde_json::json!(["build"]),
        "the run restarts from build, not from test"
    );
    assert_eq!(
        plan["expired_inputs"],
        serde_json::json!([{
            // `test` is the step whose INPUT is incomplete; `build` is the step
            // that PRODUCED the missing snapshot. Swapping the two reads as
            // "build will re-run because test's workspace expired", which is
            // backwards and, in a chain, blames a step that is downstream of the
            // problem.
            "consumer": "test",
            "produced_by": "build",
            "root": SWEPT,
        }]),
        "the diagnostic names both ends of the missing edge, and which is which"
    );
}
