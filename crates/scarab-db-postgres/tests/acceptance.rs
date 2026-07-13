//! Slice-2 ACCEPTANCE (ADR-0017, 0006, 0027): prove the whole slice end-to-end.
//!
//! A diamond `A -> {B, C} -> D` **compiled from YAML** exercises every slice-2
//! capability against *real* Postgres with a *fake* executor:
//!   1. YAML → validated IR (scarab-pipeline).
//!   2. IR + `needs` persisted; dependency-aware admission runs B and C
//!      concurrently, D last.
//!   3. Content-addressed workspace flows A → B/C → D; D sees both B's and C's
//!      outputs.
//!   4. Restart C: C and its descendant D re-run; its sibling B does not.
//!
//! The live-kind variant (real Pods doing the workspace I/O via the dev harness)
//! is `#[ignore]`-gated at the bottom. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::atomic::{AtomicU32, Ordering};

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    restart_step, Db, RunId, RunStatus, Scheduler, StepId, StepStatus, Timestamp,
};
use scarab_storage::Cas;
use scarab_storage_s3::S3Storage;
use scarab_testkit::{FakeClock, FakeExecutor};

const DIAMOND: &str = r#"
ir_version: 1
steps:
  - { id: A, image: busybox, command: ["true"] }
  - { id: B, image: busybox, needs: [A] }
  - { id: C, image: busybox, needs: [A] }
  - { id: D, image: busybox, needs: [B, C] }
"#;

static N: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("scarab-acc-{tag}-{}-{}", std::process::id(), n))
}

/// Compile the diamond YAML and durably create the run: IR stored on the run,
/// each step's `needs` persisted. Returns the compiled step list.
async fn compile_and_create(db: &PostgresDb, run: &RunId) -> Vec<scarab_pipeline::StepSpec> {
    let ir = scarab_pipeline::compile_yaml(DIAMOND).expect("diamond compiles");
    assert_eq!(ir.steps.len(), 4);

    db.create_run(run, ir.ir_version, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(run, &serde_json::to_value(&ir).unwrap()).await.unwrap();
    for step in &ir.steps {
        let spec = scarab_engine::StepSpec {
            image: step.image.clone(),
            command: step.command.clone(),
            env: step.env.clone(),
            secrets: step.secrets.clone(),
        };
        let needs: Vec<StepId> = step.needs.0.iter().map(|n| StepId(n.clone())).collect();
        db.create_step_run(run, &StepId(step.id.clone()), Some(&spec), &needs, Timestamp(0))
            .await
            .unwrap();
    }
    ir.steps
}

async fn drive_to_terminal(sched: &Scheduler<'_>, db: &PostgresDb, run: &RunId) {
    for _ in 0..10 {
        sched.tick(run).await.expect("tick");
        if db.run_status(run).await.unwrap().unwrap().is_terminal() {
            return;
        }
    }
    panic!("run did not settle within 10 ticks");
}

fn status_of(steps: &[scarab_engine::StepRun], id: &str) -> StepStatus {
    steps.iter().find(|s| s.step.0 == id).unwrap().status
}
fn attempts_of(steps: &[scarab_engine::StepRun], id: &str) -> usize {
    steps.iter().find(|s| s.step.0 == id).unwrap().attempts.len()
}

/// Compile → admit-in-order (B and C concurrent, D last) → restart C cascades to
/// D only, leaving B.
#[tokio::test]
async fn diamond_compiles_admits_concurrently_and_restart_cascades() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-1".into());
    compile_and_create(&db, &run).await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..20 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");

    // Tick once: only A (the root) runs.
    sched.tick(&run).await.expect("tick A");
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(status_of(&steps, "A"), StepStatus::Succeeded);
    assert_eq!(status_of(&steps, "B"), StepStatus::Pending);
    assert_eq!(status_of(&steps, "C"), StepStatus::Pending);

    // One admission now claims BOTH B and C — emergent parallelism.
    sched.admit(&run).await.expect("admit B+C");
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(status_of(&steps, "B"), StepStatus::Running, "B and C concurrent");
    assert_eq!(status_of(&steps, "C"), StepStatus::Running, "B and C concurrent");
    assert_eq!(status_of(&steps, "D"), StepStatus::Pending, "D waits for B and C");

    // Finish: D runs only after both B and C; the run succeeds.
    drive_to_terminal(&sched, &db, &run).await;
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));
    let steps = db.steps_of_run(&run).await.unwrap();
    for id in ["A", "B", "C", "D"] {
        assert_eq!(attempts_of(&steps, id), 1, "{id} ran once");
    }

    // Restart C: C and its descendant D re-run; sibling B and ancestor A do not.
    restart_step(&db, &clock, &run, &StepId("C".into())).await.expect("restart C");
    drive_to_terminal(&sched, &db, &run).await;
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(attempts_of(&steps, "A"), 1, "ancestor A not re-run");
    assert_eq!(attempts_of(&steps, "B"), 1, "sibling B not re-run");
    assert_eq!(attempts_of(&steps, "C"), 2, "restarted C re-ran");
    assert_eq!(attempts_of(&steps, "D"), 2, "descendant D re-ran");

    tdb.cleanup().await;
}

/// The content-addressed workspace flows along the diamond's edges: A's output
/// is inherited by B and C, whose outputs are both inherited by D — so D's input
/// workspace contains every upstream file. Executor/data-plane is simulated
/// (fakes) using a real local CAS + Postgres-recorded output snapshots.
#[tokio::test]
async fn diamond_workspace_flows_to_d_which_sees_both() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let cas = S3Storage::local(temp_dir("store")).expect("local cas");

    let run = RunId("run-ws".into());
    let steps = compile_and_create(&db, &run).await;

    // `output_of` accumulates each step's recorded output snapshot.
    let mut output_of = std::collections::HashMap::new();

    // Run each step in topological order; each inherits its needs' workspaces
    // (implicit-by-default), adds its own file, and snapshots the result.
    for step in &steps {
        let id = step.id.clone();
        let needs: Vec<StepId> = step.needs.0.iter().map(|n| StepId(n.clone())).collect();
        let work = temp_dir(&format!("work-{id}"));
        std::fs::create_dir_all(&work).unwrap();

        // init-container: materialize every inherited input workspace.
        for snap in scarab_engine::workspace_inputs(&needs, &output_of) {
            cas.materialize(&scarab_storage::TreeHash(snap), work.to_str().unwrap())
                .await
                .expect("materialize input");
        }
        // the step "runs": it writes its own output file.
        std::fs::write(work.join(format!("{id}.txt")), format!("out-{id}")).unwrap();
        // post-step: snapshot the output workspace and record it.
        let snap = cas.ingest(work.to_str().unwrap()).await.expect("ingest output");
        db.set_step_output(&run, &StepId(id.clone()), &snap.root.0).await.unwrap();
        output_of.insert(StepId(id.clone()), snap.root.0.clone());
        let _ = std::fs::remove_dir_all(&work);
    }

    // D's input workspace = everything upstream. Materialize it and check D sees
    // A's, B's and C's files.
    let d_needs = vec![StepId("B".into()), StepId("C".into())];
    let d_in = temp_dir("work-D-in");
    for snap in scarab_engine::workspace_inputs(&d_needs, &output_of) {
        cas.materialize(&scarab_storage::TreeHash(snap), d_in.to_str().unwrap())
            .await
            .unwrap();
    }
    assert_eq!(std::fs::read(d_in.join("A.txt")).unwrap(), b"out-A", "A flowed through");
    assert_eq!(std::fs::read(d_in.join("B.txt")).unwrap(), b"out-B", "D sees B");
    assert_eq!(std::fs::read(d_in.join("C.txt")).unwrap(), b"out-C", "D sees C");

    let _ = std::fs::remove_dir_all(&d_in);
    tdb.cleanup().await;
}

/// The live-kind variant: drive the same diamond on a real kind cluster from the
/// dev harness, where Pods do the workspace I/O end-to-end (init-container fetch
/// + post-step upload). Needs a cluster + object store, so it is opt-in only.
/// TODO(slice-2): wire once build_pod gains init-container/post-step workspace.
#[tokio::test]
#[ignore = "requires the dev kind cluster + object store; opt in with SCARAB_TEST_KUBE=1"]
async fn diamond_on_live_kind() {
    if std::env::var("SCARAB_TEST_KUBE").is_err() {
        return;
    }
}
