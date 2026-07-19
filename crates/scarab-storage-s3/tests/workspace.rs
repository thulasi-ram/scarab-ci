//! Workspace-along-DAG-edges acceptance (ADR-0004, 0007, 0029): a step's output
//! workspace flows to a dependent's input via the CAS. Exercised in-process with
//! a *local* CAS and the *in-memory* Db (the executor/pod is faked): step A
//! produces a file, its workspace is snapshotted to CAS and recorded on the run;
//! step B (which `needs` A) resolves its inputs and materializes them — and sees
//! the file A produced. The real cluster path (init-container fetch + post-step
//! upload) is `#[ignore]`-gated below (needs a live cluster).

use std::sync::atomic::{AtomicU32, Ordering};

use scarab_engine::{workspace_inputs, AttemptId, Db, RunId, StepId, Timestamp};
use scarab_storage::{Cas, TreeHash};
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
    db.set_step_output(&run, &a, &AttemptId("a1".into()), &snapshot.root.0)
        .await
        .unwrap();

    // --- Step B starts: resolve its input workspace from its needs, then
    //     materialize it (init-container fetch). ---
    let mut outputs = std::collections::HashMap::new();
    if let Some(h) = db.step_output(&run, &a).await.unwrap() {
        outputs.insert(a.clone(), h);
    }
    let inputs = workspace_inputs(std::slice::from_ref(&a), &outputs);
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

/// The real cluster path — a step Pod with an init-container that fetches the
/// input snapshot from CAS and a post-step wrapper that uploads outputs — needs
/// a live cluster + object store, so it is opt-in only. Placeholder guarding the
/// intended behavior; the in-process test above proves the CAS-along-edges
/// mechanic itself. TODO(slice-2): wire init-container/post-step into build_pod.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster + object store; opt in with SCARAB_TEST_KUBE=1"]
async fn cluster_workspace_round_trip() {
    if std::env::var("SCARAB_TEST_KUBE").is_err() {
        return;
    }
    // Intentionally unimplemented until build_pod gains init-container/post-step
    // workspace wiring (a follow-up in this slice).
}
