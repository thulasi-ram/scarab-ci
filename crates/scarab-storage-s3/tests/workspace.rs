//! Workspace-along-DAG-edges acceptance (ADR-0004, 0007, 0029): a step's output
//! workspace flows to a dependent's input via the CAS. Exercised in-process with
//! a *local* CAS and the *in-memory* Db (the executor/pod is faked): step A
//! produces a file, its workspace is snapshotted to CAS and recorded on the run;
//! step B (which `needs` A) resolves its inputs and materializes them — and sees
//! the file A produced. Also covers per-path publishing (`outputs:`, ADR-0007):
//! pruning a snapshot to the declared paths, its order-independence, and its
//! fail-closed diagnostics. The real cluster path (init-container fetch +
//! egress snapshot) lives in `scarab-executor-k8s/tests/cluster.rs`.

use std::sync::atomic::{AtomicU32, Ordering};

use scarab_engine::{workspace_inputs, AttemptId, Db, RunId, StepId, Timestamp};
use scarab_storage::{prune_tree, Cas, PruneError, TreeHash};
use scarab_storage_s3::S3Storage;
use scarab_testkit::InMemoryDb;

static N: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("scarab-ws-{tag}-{}-{}", std::process::id(), n))
}

/// step B sees the file step A produced, passed along the `needs` edge as a
/// content-addressed workspace snapshot.
#[tokio::test]
async fn output_workspace_flows_to_dependent_input() {
    let store_dir = temp_dir("store");
    let a_work = temp_dir("a-work");
    let b_work = temp_dir("b-work");

    let cas = S3Storage::local(&store_dir).expect("local cas");
    let db = InMemoryDb::new();

    let run = RunId("run-1".into());
    let a = StepId("A".into());
    let b = StepId("B".into());
    // B depends on A (implicit-by-default: B inherits A's output workspace).
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &a, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &b, None, std::slice::from_ref(&a), Timestamp(0))
        .await
        .unwrap();

    // --- Step A runs: it writes a file into its workspace, which is then
    //     snapshotted to CAS and recorded as A's output (post-step upload). ---
    std::fs::create_dir_all(a_work.join("out")).unwrap();
    std::fs::write(a_work.join("out/a.txt"), b"produced-by-A").unwrap();
    let snapshot = cas
        .ingest(a_work.to_str().unwrap())
        .await
        .expect("ingest A");
    db.set_step_output(&run, &a, &AttemptId("a1".into()), &snapshot.root.0, None, None)
        .await
        .unwrap();

    // --- Step B starts: resolve its input workspace from its needs, then
    //     materialize it (init-container fetch). ---
    let mut outputs = std::collections::HashMap::new();
    if let Some(h) = db.step_output(&run, &a).await.unwrap() {
        outputs.insert(a.clone(), h);
    }
    let inputs = workspace_inputs(std::slice::from_ref(&a), None, &outputs);
    assert_eq!(inputs.len(), 1, "B inherits A's one output workspace");
    cas.materialize(&TreeHash(inputs[0].clone()), b_work.to_str().unwrap())
        .await
        .expect("materialize B input");

    // B sees exactly what A produced.
    assert_eq!(
        std::fs::read(b_work.join("out/a.txt")).unwrap(),
        b"produced-by-A"
    );

    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&a_work);
    let _ = std::fs::remove_dir_all(&b_work);
}

// The real cluster path (init container fetches the input snapshot, egress
// snapshots the output) lives in `scarab-executor-k8s/tests/cluster.rs` —
// `workspace_flows_from_a_to_b_through_the_cas` and friends, run by the `kind`
// CI tier. An empty placeholder used to sit here claiming build_pod still needed
// that wiring; it has been wired since, so the placeholder was a lie.

/// Publishing a path subset (`outputs:`, ADR-0007): the dependent sees exactly
/// the declared paths and nothing else — proving the selection needs no new CAS
/// capability, just a prune-and-rebuild over the existing tree primitives.
#[tokio::test]
async fn declared_outputs_publish_only_those_paths() {
    let store_dir = temp_dir("store-prune");
    let a_work = temp_dir("a-prune");
    let b_work = temp_dir("b-prune");
    let cas = S3Storage::local(&store_dir).expect("local cas");

    // A messy workspace: the artifact we publish, a nested report, and junk.
    std::fs::create_dir_all(a_work.join("dist/inner")).unwrap();
    std::fs::create_dir_all(a_work.join("reports/junit")).unwrap();
    std::fs::create_dir_all(a_work.join("target/debug")).unwrap();
    std::fs::write(a_work.join("dist/app"), b"binary").unwrap();
    std::fs::write(a_work.join("dist/inner/lib"), b"lib").unwrap();
    std::fs::write(a_work.join("reports/junit/results.xml"), b"<xml/>").unwrap();
    std::fs::write(a_work.join("reports/noise.log"), b"noise").unwrap();
    std::fs::write(a_work.join("target/debug/huge"), b"cache").unwrap();
    std::fs::write(a_work.join("scratch.tmp"), b"tmp").unwrap();

    let full = cas.ingest(a_work.to_str().unwrap()).await.expect("ingest");
    let pruned = prune_tree(
        &cas,
        &full.root,
        &[
            "dist".to_string(),
            "reports/junit/results.xml".to_string(),
        ],
    )
    .await
    .expect("prune to the declared outputs");

    assert_ne!(pruned.0, full.root.0, "a narrower publish is a new root");
    cas.materialize(&pruned, b_work.to_str().unwrap())
        .await
        .expect("materialize the pruned tree");

    // Declared paths arrive, whole subtrees included.
    assert_eq!(std::fs::read(b_work.join("dist/app")).unwrap(), b"binary");
    assert_eq!(std::fs::read(b_work.join("dist/inner/lib")).unwrap(), b"lib");
    assert_eq!(
        std::fs::read(b_work.join("reports/junit/results.xml")).unwrap(),
        b"<xml/>"
    );
    // Everything else is absent — including siblings inside a selected path's
    // parent, which is the whole point of a precise slice.
    assert!(!b_work.join("target").exists(), "unrelated dir published");
    assert!(!b_work.join("scratch.tmp").exists(), "junk file published");
    assert!(
        !b_work.join("reports/noise.log").exists(),
        "sibling of a selected nested file published"
    );

    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&a_work);
    let _ = std::fs::remove_dir_all(&b_work);
}

/// Selecting the same paths must yield the same root regardless of authoring
/// order — the output hash is a property of the content, so `outputs: [b, a]`
/// and `outputs: [a, b]` cannot produce different cache keys.
#[tokio::test]
async fn pruning_is_order_independent_and_merges_nested_selections() {
    let store_dir = temp_dir("store-order");
    let work = temp_dir("work-order");
    let cas = S3Storage::local(&store_dir).expect("local cas");

    std::fs::create_dir_all(work.join("a/x")).unwrap();
    std::fs::write(work.join("a/x/one"), b"1").unwrap();
    std::fs::write(work.join("a/two"), b"2").unwrap();
    std::fs::write(work.join("b"), b"3").unwrap();
    let full = cas.ingest(work.to_str().unwrap()).await.expect("ingest");

    let forward = prune_tree(&cas, &full.root, &["a/x/one".into(), "a/two".into()])
        .await
        .unwrap();
    let reverse = prune_tree(&cas, &full.root, &["a/two".into(), "a/x/one".into()])
        .await
        .unwrap();
    assert_eq!(
        forward.0, reverse.0,
        "the published root must not depend on authoring order"
    );

    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&work);
}

/// Fail-closed: a declared output the step never produced is an error naming the
/// path, not a quietly narrower publish (the exact failure mode the earlier
/// "parsed but not consumed" deferral existed to avoid).
#[tokio::test]
async fn a_declared_output_that_was_not_produced_is_an_error() {
    let store_dir = temp_dir("store-missing");
    let work = temp_dir("work-missing");
    let cas = S3Storage::local(&store_dir).expect("local cas");

    std::fs::create_dir_all(work.join("dist")).unwrap();
    std::fs::write(work.join("dist/app"), b"binary").unwrap();
    let full = cas.ingest(work.to_str().unwrap()).await.expect("ingest");

    let err = prune_tree(&cas, &full.root, &["dist".into(), "coverage".into()])
        .await
        .expect_err("a missing declared path must fail");
    assert!(
        matches!(&err, PruneError::MissingPath(p) if p == "coverage"),
        "the error must name the path the author declared: {err}"
    );

    // A nested miss reports the authored path, not just the leaf.
    let err = prune_tree(&cas, &full.root, &["dist/missing".into()])
        .await
        .expect_err("a missing nested path must fail");
    assert!(
        matches!(&err, PruneError::MissingPath(p) if p == "dist/missing"),
        "nested diagnostics must be re-rooted on the authored path: {err}"
    );

    // Escaping the workspace is rejected outright.
    for bad in ["../secrets", "/etc/passwd", "."] {
        let err = prune_tree(&cas, &full.root, &[bad.to_string()])
            .await
            .expect_err("an unsafe path must fail");
        assert!(
            matches!(err, PruneError::UnsafePath(_)),
            "{bad} should be rejected as unsafe, got: {err}"
        );
    }

    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&work);
}
