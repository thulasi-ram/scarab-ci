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
    db.set_step_output(run, step, &AttemptId("a1".into()), &root.0, None)
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
async fn workspace_names_a_symlink_and_flags_its_target_stream() {
    // git-bug 1344d1d: a symlink is a blob-holding-the-target-path marked by
    // the mode file-type bits (ADR-0061 s7). The listing must say "symlink"
    // (with the target), never "file" — and the file endpoint, whose body for
    // a symlink IS the target path, must flag that so no client presents a
    // path as file contents.
    let db = Arc::new(InMemoryDb::new());
    let cas = MemCas::default();
    let run = RunId("r1".into());
    let step = StepId("build".into());
    seed_run_step(&db, &run, &step).await;

    let readme = cas.put_blob(b"# real file\n").await.unwrap();
    let link_target = cas.put_blob(b"docs/README.md").await.unwrap();
    let root = cas
        .put_tree(vec![
            TreeEntry::new("README.md", TreeTarget::Blob(readme)),
            TreeEntry::symlink("README.link", link_target),
        ])
        .await
        .unwrap();
    db.set_step_output(&run, &step, &AttemptId("a1".into()), &root.0, None)
        .await
        .unwrap();

    let app = router(state(db, Some(Arc::new(cas))));

    // Listing: the symlink is named for what the filesystem held, target inline.
    let resp = app
        .clone()
        .oneshot(get("/v1/runs/r1/steps/build/workspace"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    let entries = v["entries"].as_array().unwrap();
    assert_eq!(entries[0]["name"], "README.link");
    assert_eq!(entries[0]["kind"], "symlink");
    assert_eq!(entries[0]["target"], "docs/README.md");
    assert_eq!(entries[1]["name"], "README.md");
    assert_eq!(entries[1]["kind"], "file");
    assert!(
        entries[1].get("target").is_none(),
        "a plain file carries no target"
    );

    // File endpoint on the symlink: body is the target path, flagged as such.
    let resp = app
        .clone()
        .oneshot(get(
            "/v1/runs/r1/steps/build/workspace/file?path=README.link",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-scarab-symlink")
            .map(|h| h.to_str().unwrap()),
        Some("1"),
        "a symlink stream must be flagged"
    );
    assert_eq!(body_bytes(resp).await, b"docs/README.md");

    // …and a regular file is NOT flagged.
    let resp = app
        .oneshot(get("/v1/runs/r1/steps/build/workspace/file?path=README.md"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-scarab-symlink").is_none());
    assert_eq!(body_bytes(resp).await, b"# real file\n");
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
            outcome: AttemptOutcome::Succeeded,
        },
    )
    .await
    .unwrap();
    // The winning attempt's snapshot.
    db.set_step_output(&run, &step, &AttemptId("a2".into()), "root-2", None)
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
    // The per-attempt evidence field this endpoint serves (kills a DTO that
    // stores it but maps `None`/drops it in `attempt_dto`): the failed
    // attempt's human-readable cause — absent (skip_serializing_if) where it
    // does not apply. (`output_durability` was its sibling until ADR-0067
    // part 4 made durability a property of the drain and dropped the column.)
    assert_eq!(
        al[0]["failure_detail"], "cold tier refused: connection refused",
        "the stored diagnosis is finally served, not write-only"
    );
    assert!(
        al[1]["failure_detail"].is_null(),
        "no diagnosis on the winning attempt"
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
async fn step_logs_error_when_indexed_chunks_will_not_read() {
    // ADR-0063 part 5: absence is authoritative ONLY when the index says no
    // chunks. Here the index promises chunks whose bytes are gone (a pod roll,
    // or another replica's emptyDir) — the reader must be told, never handed a
    // plausible blank pane.
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("build".into());
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
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Succeeded,
        },
    )
    .await
    .unwrap();

    // Index a chunk through one store, then serve through a LogService whose
    // store never held the bytes (same shared index).
    let writer = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    writer
        .append(
            &run,
            &step,
            &AttemptId("a1".into()),
            b"the bytes that go missing\n",
        )
        .await
        .unwrap();
    let serving = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db, Arc::new(FakeClock::new(1_000)), serving));

    // Per-step route: a real error naming log storage, not an empty pane.
    let resp = app
        .clone()
        .oneshot(get("/v1/runs/r1/steps/build/logs"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(body.contains("log storage"), "{body}");

    // Run-wide route: same rule.
    let resp = app.oneshot(get("/v1/runs/r1/logs")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn step_logs_with_no_indexed_chunks_stay_a_truthful_empty_pane() {
    // The other branch of the rule: the index records NO chunks, so the step
    // genuinely printed nothing — an empty 200, not an error.
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("build".into());
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
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Succeeded,
        },
    )
    .await
    .unwrap();
    let app = router(state(db, None));

    let resp = app
        .oneshot(get("/v1/runs/r1/steps/build/logs"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(body.is_empty(), "no chunks means an empty replay: {body:?}");
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
