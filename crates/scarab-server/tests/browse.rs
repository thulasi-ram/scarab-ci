//! Run-detail browse endpoints (the Inspector): the READ side of step results
//! (ADR-0041) and the read-only workspace-snapshot browser (ADR-0029). Proves the
//! results GET returns typed values, the workspace browser walks a merkle tree
//! (directories first), streams a file, refuses `..` traversal, reports
//! `available:false` for a step with no snapshot, and 404s when browse is
//! disabled. Hermetic (InMemoryDb + a tiny in-test CAS, no network).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Db, FailureKind, RunId, RunStatus, StepId, Timestamp,
};
use scarab_server::{router, AppState, LogService};
use scarab_storage::{BlobHash, Cas, Snapshot, StorageError, TreeEntry, TreeHash, TreeTarget};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

// ── a trivial content-addressed store for the test ─────────────────────────
#[derive(Default)]
struct MemCas {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
    trees: Mutex<HashMap<String, Vec<TreeEntry>>>,
}

fn key(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[async_trait]
impl Cas for MemCas {
    async fn put_blob(&self, data: &[u8]) -> Result<BlobHash, StorageError> {
        let k = key(data);
        self.blobs.lock().unwrap().insert(k.clone(), data.to_vec());
        Ok(BlobHash(k))
    }
    async fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>, StorageError> {
        self.blobs
            .lock()
            .unwrap()
            .get(&hash.0)
            .cloned()
            .ok_or(StorageError::NotFound)
    }
    async fn put_tree(&self, entries: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
        let k = key(format!("{entries:?}").as_bytes());
        self.trees.lock().unwrap().insert(k.clone(), entries);
        Ok(TreeHash(k))
    }
    async fn tree_entries(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
        self.trees
            .lock()
            .unwrap()
            .get(&hash.0)
            .cloned()
            .ok_or(StorageError::NotFound)
    }
    async fn materialize(&self, _tree: &TreeHash, _path: &str) -> Result<(), StorageError> {
        unreachable!("browse is read-only")
    }
    async fn ingest(&self, _path: &str) -> Result<Snapshot, StorageError> {
        unreachable!("browse is read-only")
    }
}

fn state(db: Arc<InMemoryDb>, cas: Option<Arc<dyn Cas>>) -> AppState {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let mut st = AppState::new(db, Arc::new(FakeClock::new(1_000)), logs);
    if let Some(c) = cas {
        st = st.with_workspace_cas(c);
    }
    st
}

async fn seed_run_step(db: &InMemoryDb, run: &RunId, step: &StepId) {
    db.create_run(run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(run, step, None, &[], Timestamp(0))
        .await
        .unwrap();
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn step_results_read_back_typed() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("build".into());
    seed_run_step(&db, &run, &step).await;
    let mut results = std::collections::BTreeMap::new();
    results.insert(
        "image".to_string(),
        serde_json::json!("ghcr.io/x@sha256:9f"),
    );
    results.insert("tests_passed".to_string(), serde_json::json!(214));
    db.set_step_results(&run, &step, &AttemptId("a1".into()), &results)
        .await
        .unwrap();

    let app = router(state(db, None));
    let resp = app
        .oneshot(get("/v1/runs/r1/steps/build/results"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // BTreeMap order → "image" before "tests_passed"; type_name is the JSON kind.
    assert_eq!(arr[0]["name"], "image");
    assert_eq!(arr[0]["type_name"], "string");
    assert_eq!(arr[1]["name"], "tests_passed");
    assert_eq!(arr[1]["type_name"], "number");
    assert_eq!(arr[1]["value"], serde_json::json!(214));
}

#[tokio::test]
async fn step_results_unknown_run_is_404() {
    let db = Arc::new(InMemoryDb::new());
    let app = router(state(db, None));
    let resp = app
        .oneshot(get("/v1/runs/ghost/steps/build/results"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Build root/{crates/{Cargo.toml}, ci.yaml} in the CAS and pin it as `build`'s
/// output snapshot. Returns the shared app + CAS-backed state.
async fn seed_workspace(
    db: &Arc<InMemoryDb>,
    cas: &MemCas,
    run: &RunId,
    step: &StepId,
) -> TreeHash {
    let ci = cas.put_blob(b"on: push\n").await.unwrap();
    let cargo = cas.put_blob(b"[package]\nname = \"x\"\n").await.unwrap();
    let crates = cas
        .put_tree(vec![TreeEntry::new("Cargo.toml", TreeTarget::Blob(cargo))])
        .await
        .unwrap();
    let root = cas
        .put_tree(vec![
            TreeEntry::new("ci.yaml", TreeTarget::Blob(ci)),
            TreeEntry::new("crates", TreeTarget::Tree(crates)),
        ])
        .await
        .unwrap();
    db.set_step_output(run, step, &AttemptId("a1".into()), &root.0, None, None)
        .await
        .unwrap();
    root
}

#[tokio::test]
async fn workspace_lists_directories_first_and_streams_a_file() {
    let db = Arc::new(InMemoryDb::new());
    let cas = MemCas::default();
    let run = RunId("r1".into());
    let step = StepId("build".into());
    seed_run_step(&db, &run, &step).await;
    seed_workspace(&db, &cas, &run, &step).await;

    let app = router(state(db, Some(Arc::new(cas))));

    // Root listing: dir ("crates") sorts before file ("ci.yaml").
    let resp = app
        .clone()
        .oneshot(get("/v1/runs/r1/steps/build/workspace"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["available"], true);
    let entries = v["entries"].as_array().unwrap();
    assert_eq!(entries[0]["name"], "crates");
    assert_eq!(entries[0]["kind"], "dir");
    assert_eq!(entries[1]["name"], "ci.yaml");
    assert_eq!(entries[1]["kind"], "file");

    // Descend into the sub-tree.
    let resp = app
        .clone()
        .oneshot(get("/v1/runs/r1/steps/build/workspace?path=crates"))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["entries"][0]["name"], "Cargo.toml");

    // Stream a file's bytes.
    let resp = app
        .clone()
        .oneshot(get("/v1/runs/r1/steps/build/workspace/file?path=ci.yaml"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"on: push\n");

    // `..` traversal is refused before it can escape the snapshot root.
    let resp = app
        .oneshot(get("/v1/runs/r1/steps/build/workspace/file?path=../secret"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn workspace_without_snapshot_is_available_false() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("build".into());
    seed_run_step(&db, &run, &step).await; // no set_step_output
    let app = router(state(db, Some(Arc::new(MemCas::default()))));
    let resp = app
        .oneshot(get("/v1/runs/r1/steps/build/workspace"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["available"], false);
    assert!(v["entries"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn run_detail_exposes_attempt_list_for_reruns() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("test".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &step, None, &[], Timestamp(0))
        .await
        .unwrap();
    // Attempt 1 failed on infra, attempt 2 succeeded — the retry story.
    db.record_attempt(
        &run,
        &step,
        &Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(10),
            failure: Some(FailureKind::Infra {
                never_started: false,
            }),
            // The executor's diagnosis (4cf03d7): stored since migration 0041,
            // SERVED since ADR-0064 s2 — this test is what proves the serve.
            failure_detail: Some("cold tier refused: connection refused".into()),
            output_durability: None,
            outcome: AttemptOutcome::Failed,
        },
    )
    .await
    .unwrap();
    db.record_attempt(
        &run,
        &step,
        &Attempt {
            id: AttemptId("a2".into()),
            started_at: Timestamp(20),
            failure: None,
            failure_detail: None,
            output_durability: None,
            outcome: AttemptOutcome::Succeeded,
        },
    )
    .await
    .unwrap();
    // The winning attempt's snapshot, stamped with the durability tier the
    // Depot reported at flush time (ADR-0064 s2) — the write path the
    // scheduler uses, so the read below covers store → engine → DTO.
    db.set_step_output(
        &run,
        &step,
        &AttemptId("a2".into()),
        "root-2",
        None,
        Some("object"),
    )
    .await
    .unwrap();

    let app = router(state(db, None));
    let resp = app.oneshot(get("/v1/runs/r1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    let s0 = &v["steps"][0];
    assert_eq!(s0["attempts"], 2, "count preserved");
    let al = s0["attempt_list"].as_array().unwrap();
    assert_eq!(al.len(), 2);
    assert_eq!(al[0]["failed"], true);
    assert_eq!(al[0]["failure"], "infra");
    assert_eq!(al[1]["failed"], false);
    assert!(
        al[1].get("failure").is_none() || al[1]["failure"].is_null(),
        "no failure on the winning attempt"
    );
    // The two per-attempt evidence fields ADR-0064 s2 serves (kills a DTO that
    // stores them but maps `None`/drops them in `attempt_dto`): the failed
    // attempt's human-readable cause, the winning attempt's durability tier —
    // and each absent (skip_serializing_if) where it does not apply.
    assert_eq!(
        al[0]["failure_detail"], "cold tier refused: connection refused",
        "the stored diagnosis is finally served, not write-only"
    );
    assert!(
        al[0]["output_durability"].is_null(),
        "a failed attempt licensed nothing — no tier to report"
    );
    assert!(
        al[1]["failure_detail"].is_null(),
        "no diagnosis on the winning attempt"
    );
    assert_eq!(
        al[1]["output_durability"], "object",
        "the tier stamped at flush time is served per attempt"
    );
}

#[tokio::test]
async fn step_logs_scope_by_attempt() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("build".into());
    // Terminal run so the SSE stream replays and closes (no live tail).
    db.seed_run(&run, RunStatus::Succeeded);
    db.create_step_run(&run, &step, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.record_attempt(
        &run,
        &step,
        &Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(10),
            failure: Some(FailureKind::Step),
            failure_detail: None,
            output_durability: None,
            outcome: AttemptOutcome::Failed,
        },
    )
    .await
    .unwrap();
    db.record_attempt(
        &run,
        &step,
        &Attempt {
            id: AttemptId("a2".into()),
            started_at: Timestamp(20),
            failure: None,
            failure_detail: None,
            output_durability: None,
            outcome: AttemptOutcome::Succeeded,
        },
    )
    .await
    .unwrap();

    // Append distinct output per attempt through the same LogService the app uses.
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    logs.append(
        &run,
        &step,
        &AttemptId("a1".into()),
        b"boom: failed attempt\n",
    )
    .await
    .unwrap();
    logs.append(&run, &step, &AttemptId("a2".into()), b"ok: retry passed\n")
        .await
        .unwrap();
    let app = router(AppState::new(db, Arc::new(FakeClock::new(1_000)), logs));

    // Whole step = both attempts.
    let resp = app
        .clone()
        .oneshot(get("/v1/runs/r1/steps/build/logs"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(body.contains("boom: failed attempt") && body.contains("ok: retry passed"));

    // Scoped to just the failed attempt.
    let resp = app
        .clone()
        .oneshot(get("/v1/runs/r1/steps/build/logs?attempt=a1"))
        .await
        .unwrap();
    let body = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(body.contains("boom: failed attempt"));
    assert!(
        !body.contains("ok: retry passed"),
        "attempt scope excludes the retry"
    );

    // Unknown attempt / unknown step → 404.
    let resp = app
        .clone()
        .oneshot(get("/v1/runs/r1/steps/build/logs?attempt=ghost"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app
        .oneshot(get("/v1/runs/r1/steps/ghost/logs"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn workspace_browse_disabled_without_cas_is_404() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("build".into());
    seed_run_step(&db, &run, &step).await;
    let app = router(state(db, None)); // no workspace CAS wired
    let resp = app
        .oneshot(get("/v1/runs/r1/steps/build/workspace"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
