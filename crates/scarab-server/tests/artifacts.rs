//! Artifact list + download API (ADR-0052), hermetic: metadata rows +
//! object-store blobs seeded; list and download are Project-scoped like
//! every run read (cross-tenant denial).
//!
//! Plus the INDEXING half (98ea804): a step that published an artifact must be
//! visible through the API after the real scheduler drove it over the real
//! decorated executor stack — the uploaded-implies-indexed invariant.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::{ExecState, FailureClass};
use scarab_engine::{
    ArtifactMeta, AttemptId, Clock, Db, Executor, RunId, RunStatus, Scheduler, StepId, StepSpec,
    Timestamp,
};
use scarab_server::clone_executor::CloneEnrichingExecutor;
use scarab_server::{router, AppState, LogService, SecretInjectingExecutor};
use scarab_storage::ObjectStore;
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, FakeSecrets, InMemoryDb, InMemoryObjectStore,
};

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn artifacts_are_listed_and_downloadable() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));

    let run = RunId("r1".into());
    db.seed_run(&run, RunStatus::Succeeded);
    store
        .put("artifacts/r1/dist/report.html", b"<h1>ok</h1>".to_vec())
        .await
        .unwrap();
    db.put_artifacts(
        &run,
        &StepId("build".into()),
        &AttemptId("a1".into()),
        true,
        &[ArtifactMeta {
            name: "dist/report.html".into(),
            size: 11,
            content_type: "text/html".into(),
            object_key: "artifacts/r1/dist/report.html".into(),
        }],
        Timestamp(1),
    )
    .await
    .unwrap();

    let app = router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
            .with_artifact_store(store.clone()),
    );

    // List.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v[0]["name"], "dist/report.html");
    assert_eq!(v[0]["size"], 11);
    assert_eq!(v[0]["content_type"], "text/html");

    // Download (a slash-carrying name).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/dist/report.html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/html")
    );
    assert_eq!(body_bytes(resp).await, b"<h1>ok</h1>");

    // Unknown artifact / unknown run.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/nope.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/nope/artifacts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// ADR-0056: a retry never destroys a prior attempt's artifact version. The
/// failed attempt's `report.txt` (the evidence of what the retry recovered
/// from) stays listed and downloadable by exact version; the bare
/// name-addressed download resolves to the latest SUCCESSFUL version.
#[tokio::test]
async fn artifact_versions_are_immutable_per_attempt_and_resolve_of_record() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));

    let run = RunId("r1".into());
    let step = StepId("test".into());
    db.seed_run(&run, RunStatus::Succeeded);
    for (attempt, succeeded, body) in [("a1", false, "FAILED 3 tests"), ("a2", true, "all green")] {
        let key = format!("artifacts/r1/{attempt}/report.txt");
        store.put(&key, body.as_bytes().to_vec()).await.unwrap();
        db.put_artifacts(
            &run,
            &step,
            &AttemptId(attempt.into()),
            succeeded,
            &[ArtifactMeta {
                name: "report.txt".into(),
                size: body.len() as u64,
                content_type: "text/plain".into(),
                object_key: key,
            }],
            Timestamp(if attempt == "a1" { 1 } else { 2 }),
        )
        .await
        .unwrap();
    }

    let app = router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
            .with_artifact_store(store.clone()),
    );

    // Both versions listed, tagged with provenance; only a2 is of-record.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 2, "the failed attempt's version is retained");
    let a1 = rows.iter().find(|r| r["attempt"] == "a1").unwrap();
    let a2 = rows.iter().find(|r| r["attempt"] == "a2").unwrap();
    assert_eq!(a1["succeeded"], false);
    assert_eq!(a1["of_record"], false);
    assert_eq!(a2["succeeded"], true);
    assert_eq!(a2["of_record"], true);

    // Bare name = of-record = the successful attempt's bytes.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/report.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"all green");

    // Pinned version = the failed attempt's evidence, still readable.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/report.txt?step=test&attempt=a1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"FAILED 3 tests");
}

/// ADR-0056: when EVERY version of a name failed, the bare name-addressed
/// download refuses (404) rather than silently serving a failed attempt's
/// partial file — the consumer must opt into a pinned version to read it.
#[tokio::test]
async fn of_record_never_silently_serves_a_failed_version() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));

    let run = RunId("r1".into());
    db.seed_run(&run, RunStatus::Failed);
    store
        .put("artifacts/r1/a1/core.dump", b"x".to_vec())
        .await
        .unwrap();
    db.put_artifacts(
        &run,
        &StepId("test".into()),
        &AttemptId("a1".into()),
        false,
        &[ArtifactMeta {
            name: "core.dump".into(),
            size: 1,
            content_type: "application/octet-stream".into(),
            object_key: "artifacts/r1/a1/core.dump".into(),
        }],
        Timestamp(1),
    )
    .await
    .unwrap();

    let app = router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
            .with_artifact_store(store.clone()),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/core.dump")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "no successful version"
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/core.dump?step=test&attempt=a1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "pinned read still works");
}

/// Feature acceptance for 98ea804 (ADR-0052 + ADR-0017): an artifact whose blob
/// the backend already uploaded is ALWAYS indexed, and therefore listed and
/// downloadable — driven through the **production executor stack**, i.e. the
/// decorators the server actually wraps the backend in
/// (`CloneEnrichingExecutor` → `SecretInjectingExecutor` → backend), the real
/// router and the real `Scheduler`.
///
/// The stack is the point. `Executor::artifacts` has an EMPTY trait default and
/// neither decorator forwarded it, so in every real deployment the scheduler saw
/// "this step published nothing" for every step: the blobs sat in the object
/// store, the k8s Pod annotation held the harvested index, `put_artifacts` was
/// never called, and `GET /v1/runs/{id}/artifacts` returned `[]` forever. A test
/// against the bare backend would have passed throughout — the loss lives in the
/// composition, so the test composes.
#[tokio::test]
async fn published_artifacts_are_indexed_through_the_decorated_executor_stack() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));

    // The backend that harvests artifacts (the k8s executor's role here).
    let backend = Arc::new(FakeExecutor::new());
    // The decorator chain `main.rs` builds around it, in the same order.
    let secret_exec: Arc<dyn Executor> = Arc::new(SecretInjectingExecutor::new(
        backend.clone(),
        db.clone() as Arc<dyn Db>,
        Arc::new(FakeSecrets::new()),
        logs.clone(),
    ));
    let exec: Arc<dyn Executor> = Arc::new(CloneEnrichingExecutor::new(
        secret_exec,
        db.clone(),
        Arc::new(FakeForge::new()),
    ));

    let app = router(
        AppState::new(db.clone(), clock.clone(), logs.clone()).with_artifact_store(store.clone()),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "pipeline": {
                            "ir_version": 1,
                            "steps": [{
                                "id": "build",
                                "image": "busybox",
                                "command": ["true"],
                                "artifacts": ["dist/*"]
                            }]
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    let run = RunId(created["id"].as_str().unwrap().to_string());

    // The backend's post-step harvest: the blob is uploaded FIRST (the effect
    // that has already happened), then reported as the step's harvested index.
    let key = format!("artifacts/{}/dist/report.html", run.0);
    store.put(&key, b"<h1>ok</h1>".to_vec()).await.unwrap();
    backend.set_artifacts(
        "build",
        vec![ArtifactMeta {
            name: "dist/report.html".into(),
            size: 11,
            content_type: "text/html".into(),
            object_key: key,
        }],
    );

    for _ in 0..4 {
        backend.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    for _ in 0..4 {
        sched.tick_all().await.unwrap();
    }
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded),
        "the run settled — so the artifact index had its one chance to be written"
    );

    // The user-visible claim: it is listed, of-record, with its provenance.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{}/artifacts", run.0))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    let rows = listed.as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the uploaded artifact must be indexed, not silently dropped: {listed}"
    );
    assert_eq!(rows[0]["name"], "dist/report.html");
    assert_eq!(rows[0]["size"], 11);
    assert_eq!(rows[0]["step"], "build");
    assert_eq!(rows[0]["succeeded"], true);
    assert_eq!(rows[0]["of_record"], true);

    // …and downloadable, which is what the index exists for.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{}/artifacts/dist/report.html", run.0))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"<h1>ok</h1>");
}

/// Feature acceptance for a28a173 (ADR-0052 + ADR-0056 + ADR-0017): a step that
/// exits NON-ZERO still has its artifacts indexed, listed and downloadable.
///
/// A failing attempt's artifacts are often THE evidence — the JUnit XML of the
/// suite that just went red, the crashed build's log bundle, the screenshot of
/// the browser run that died. The k8s harvest gated itself on `exit == 0`, so on
/// the real backend they were silently discarded and the scheduler's
/// `ExecState::Failed` harvest branch never fired. This drives that branch end to
/// end: real router, real `Scheduler`, the production decorator chain
/// (`CloneEnrichingExecutor` → `SecretInjectingExecutor` → backend).
///
/// Note the of-record contract that goes with it (ADR-0056): the version is
/// tagged `succeeded: false`, so the bare name-addressed download refuses rather
/// than pass a red run's partial file off as the answer — the evidence is read by
/// pinning the attempt.
#[tokio::test]
async fn a_failing_steps_artifacts_are_indexed_and_downloadable() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));

    let backend = Arc::new(FakeExecutor::new());
    let secret_exec: Arc<dyn Executor> = Arc::new(SecretInjectingExecutor::new(
        backend.clone(),
        db.clone() as Arc<dyn Db>,
        Arc::new(FakeSecrets::new()),
        logs.clone(),
    ));
    let exec: Arc<dyn Executor> = Arc::new(CloneEnrichingExecutor::new(
        secret_exec,
        db.clone(),
        Arc::new(FakeForge::new()),
    ));

    let app = router(
        AppState::new(db.clone(), clock.clone(), logs.clone()).with_artifact_store(store.clone()),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "pipeline": {
                            "ir_version": 1,
                            "steps": [{
                                "id": "test",
                                "image": "busybox",
                                "command": ["false"],
                                "artifacts": ["junit.xml"]
                            }]
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    let run = RunId(created["id"].as_str().unwrap().to_string());

    // The backend's post-step harvest of a RED step: the blob is uploaded first
    // (the effect that has already happened), then reported as its index.
    let key = format!("artifacts/{}/junit.xml", run.0);
    let body = b"<testsuite failures=\"3\"/>".to_vec();
    store.put(&key, body.clone()).await.unwrap();
    backend.set_artifacts(
        "test",
        vec![ArtifactMeta {
            name: "junit.xml".into(),
            size: body.len() as u64,
            content_type: "application/xml".into(),
            object_key: key,
        }],
    );

    // The step's own verdict: exit 1, classified as the developer's failure —
    // not retried, so the run goes red on this attempt's evidence.
    for _ in 0..4 {
        backend.script_outcome(ExecState::Failed {
            exit_code: Some(1),
            class: FailureClass::Step,
        });
    }
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    for _ in 0..4 {
        sched.tick_all().await.unwrap();
    }
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Failed),
        "the run reports failed — and the index had its one chance to be written"
    );

    // Listed, with the provenance that says it came from a red attempt.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{}/artifacts", run.0))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    let rows = listed.as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "a failed step's artifacts are evidence, not garbage: {listed}"
    );
    assert_eq!(rows[0]["name"], "junit.xml");
    assert_eq!(rows[0]["step"], "test");
    assert_eq!(rows[0]["succeeded"], false);
    assert_eq!(rows[0]["of_record"], false);
    let attempt = rows[0]["attempt"].as_str().unwrap().to_string();
    assert!(!attempt.is_empty(), "keyed per attempt (ADR-0056)");

    // Downloadable — by pinned version, which is the only honest read of a red
    // attempt's file (ADR-0056: the bare name refuses).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/v1/runs/{}/artifacts/junit.xml?step=test&attempt={attempt}",
                    run.0
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, body);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{}/artifacts/junit.xml", run.0))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "no successful version exists — the bare name must not serve the red one"
    );
}

/// The same invariant with **real Postgres** as the collaborator (ADR-0017): the
/// index the API reads is a durable row, not an in-memory convenience, so the
/// write the decorated stack lost must be proven to land in the real store —
/// keyed per attempt (ADR-0056) as the scheduler minted it.
#[tokio::test]
async fn published_artifacts_are_durably_indexed_in_postgres() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();

    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), pg.clone()));

    let run = RunId("r-artifact-index".into());
    let step = StepId("build".into());
    pg.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    let spec = StepSpec {
        image: "busybox".into(),
        command: vec!["true".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        clone: None,
        build: None,
        // The feature: this step publishes artifacts of record (ADR-0052).
        artifacts: vec!["dist/*".into()],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    };
    pg.create_step_run(&run, &step, Some(&spec), &[], Timestamp(0))
        .await
        .unwrap();

    // Blob uploaded by the backend's harvest, then reported as the step's index.
    let key = format!("artifacts/{}/dist/report.html", run.0);
    store.put(&key, b"<h1>ok</h1>".to_vec()).await.unwrap();
    let backend = Arc::new(FakeExecutor::new());
    backend.set_artifacts(
        "build",
        vec![ArtifactMeta {
            name: "dist/report.html".into(),
            size: 11,
            content_type: "text/html".into(),
            object_key: key,
        }],
    );

    // The production decorator chain, over real Postgres.
    let secret_exec: Arc<dyn Executor> = Arc::new(SecretInjectingExecutor::new(
        backend.clone(),
        pg.clone() as Arc<dyn Db>,
        Arc::new(FakeSecrets::new()),
        logs.clone(),
    ));
    let exec: Arc<dyn Executor> = Arc::new(CloneEnrichingExecutor::new(
        secret_exec,
        pg.clone(),
        Arc::new(FakeForge::new()),
    ));

    for _ in 0..4 {
        backend.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(pg.as_ref(), &*clock as &dyn Clock, &*exec, "sched");
    for _ in 0..4 {
        sched.tick(&run).await.unwrap();
    }
    assert_eq!(
        pg.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );

    // The durable row exists, attributed to the attempt that produced it.
    let indexed = pg.artifacts_of_run(&run).await.unwrap();
    assert_eq!(
        indexed.len(),
        1,
        "the uploaded artifact must be indexed durably: {indexed:?}"
    );
    assert_eq!(indexed[0].meta.name, "dist/report.html");
    assert_eq!(indexed[0].step, step);
    assert!(indexed[0].succeeded);
    assert!(
        !indexed[0].attempt.0.is_empty(),
        "keyed per attempt (ADR-0056)"
    );

    // …and it is what the API serves.
    let app = router(
        AppState::new(pg.clone(), clock.clone(), logs.clone()).with_artifact_store(store.clone()),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{}/artifacts/dist/report.html", run.0))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"<h1>ok</h1>");

    tdb.cleanup().await;
}
