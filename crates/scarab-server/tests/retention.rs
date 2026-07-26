//! Retention sweep (ADR-0050) against *real* Postgres: only TERMINAL runs
//! past the TTL are prunable; a gate-suspended run is never eligible
//! regardless of age; blobs go first, index second, run metadata survives.
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.
//!
//! Also the home of the ADR-0061 s5 acceptance tests over the **cold tier's**
//! two escape hatches — the manual **pin** (a sweep must skip a pinned Run's
//! whole tree) and **graceful degradation** (an expired input widens a rerun
//! instead of failing a Step). Both are exercised against a real sweeper and a
//! real CAS rather than a stubbed presence check, because the whole question is
//! whether the bytes are actually gone.

mod common;

use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{
    AttemptId, Clock, Db, LogChunkMeta, RunId, RunStatus, StepId, StepStatus, Timestamp,
};
use scarab_server::retention::{sweep_retention, RetentionConfig};
use scarab_storage::ObjectStore;
use scarab_testkit::{FakeClock, InMemoryObjectStore};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

async fn seed_run_with_log(
    db: &PostgresDb,
    store: &InMemoryObjectStore,
    id: &str,
    status_path: &[(RunStatus, RunStatus)],
    at: Timestamp,
) {
    let run = RunId(id.into());
    db.create_run(&run, 1, 1, at).await.unwrap();
    db.append_event(&scarab_engine::EventKind {
        version: scarab_engine::EVENT_VERSION,
        run: run.clone(),
        kind: scarab_engine::EventPayload::RunCreated,
        at,
    })
    .await
    .unwrap();
    for (from, to) in status_path {
        db.record_transition(&run, *from, *to).await.unwrap();
    }
    let key = format!("logs/{id}/s1/a1/0");
    store.put(&key, b"log-bytes".to_vec()).await.unwrap();
    db.append_log_chunk(
        &run,
        &StepId("s1".into()),
        &scarab_engine::AttemptId("a1".into()),
        &LogChunkMeta {
            seq: 0,
            byte_offset: 0,
            len: 9,
            object_key: key,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn sweeps_only_terminal_runs_past_ttl_and_keeps_metadata() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let store = Arc::new(InMemoryObjectStore::new());
    let old = Timestamp(0);

    // 1. An OLD TERMINAL run — prunable.
    seed_run_with_log(
        &db,
        &store,
        "r-old-done",
        &[
            (RunStatus::Pending, RunStatus::Running),
            (RunStatus::Running, RunStatus::Succeeded),
        ],
        old,
    )
    .await;
    // 2. An OLD run SUSPENDED on a gate — never prunable, regardless of age.
    seed_run_with_log(
        &db,
        &store,
        "r-old-suspended",
        &[
            (RunStatus::Pending, RunStatus::Running),
            (RunStatus::Running, RunStatus::Suspended),
        ],
        old,
    )
    .await;

    // updated_at is stamped by the transitions above (wall-clock "now"), so
    // age the terminal run's row explicitly to simulate 40 days of quiet.
    sqlx::query("UPDATE runs SET updated_at = 0 WHERE id = 'r-old-done'")
        .execute(&tdb.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE runs SET updated_at = 0 WHERE id = 'r-old-suspended'")
        .execute(&tdb.pool)
        .await
        .unwrap();

    // 3. A FRESH terminal run — within TTL, kept.
    seed_run_with_log(
        &db,
        &store,
        "r-fresh-done",
        &[
            (RunStatus::Pending, RunStatus::Running),
            (RunStatus::Running, RunStatus::Failed),
        ],
        old,
    )
    .await;

    // The old terminal run also holds an ARTIFACT (its own class, 20d TTL
    // here so it is also due); the suspended run's artifact must survive.
    for id in ["r-old-done", "r-old-suspended"] {
        store
            .put(&format!("artifacts/{id}/report.txt"), b"r".to_vec())
            .await
            .unwrap();
        db.put_artifacts(
            &RunId(id.into()),
            &StepId("s1".into()),
            &AttemptId("a1".into()),
            true,
            &[scarab_engine::ArtifactMeta {
                name: "report.txt".into(),
                size: 1,
                content_type: "text/plain".into(),
                object_key: format!("artifacts/{id}/report.txt"),
            }],
            old,
        )
        .await
        .unwrap();
    }

    // Sweep at day 40 with a 30-day TTL.
    let db: Arc<dyn Db> = Arc::new(db);
    let store_dyn: Arc<dyn ObjectStore> = store.clone();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(40 * DAY_MS));
    let pruned = sweep_retention(
        &db,
        &store_dyn,
        &clock,
        "sweeper-1",
        RetentionConfig {
            log_ttl_ms: 30 * DAY_MS,
            artifact_ttl_ms: 20 * DAY_MS,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        pruned, 2,
        "the old terminal run's logs AND artifacts classes"
    );
    // The artifact class: old-done pruned (blob + rows), suspended kept.
    assert!(store.get("artifacts/r-old-done/report.txt").await.is_err());
    assert!(db
        .artifacts_of_run(&RunId("r-old-done".into()))
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .get("artifacts/r-old-suspended/report.txt")
        .await
        .is_ok());

    // The pruned run: blobs gone, index gone — metadata retained.
    let gone = RunId("r-old-done".into());
    assert!(
        store.get("logs/r-old-done/s1/a1/0").await.is_err(),
        "blob deleted"
    );
    assert!(
        db.log_object_keys_of_run(&gone).await.unwrap().is_empty(),
        "index dropped"
    );
    assert_eq!(
        db.run_status(&gone).await.unwrap(),
        Some(RunStatus::Succeeded),
        "run metadata survives its blobs (ADR-0050)"
    );
    assert!(
        !db.events(&gone).await.unwrap().is_empty(),
        "event log retained"
    );

    // The suspended run — same age — is untouched (lifecycle-keyed).
    assert!(store.get("logs/r-old-suspended/s1/a1/0").await.is_ok());
    assert_eq!(
        db.log_object_keys_of_run(&RunId("r-old-suspended".into()))
            .await
            .unwrap()
            .len(),
        1
    );

    // The fresh terminal run is untouched (within TTL).
    assert_eq!(
        db.log_object_keys_of_run(&RunId("r-fresh-done".into()))
            .await
            .unwrap()
            .len(),
        1
    );

    // Idempotent: a second sweep finds nothing.
    let pruned = sweep_retention(
        &db,
        &store_dyn,
        &clock,
        "sweeper-1",
        RetentionConfig {
            log_ttl_ms: 30 * DAY_MS,
            artifact_ttl_ms: 20 * DAY_MS,
        },
    )
    .await
    .unwrap();
    assert_eq!(pruned, 0);

    // A NON-leader replica sweeps nothing while the lease is held.
    let pruned = sweep_retention(
        &db,
        &store_dyn,
        &clock,
        "sweeper-2",
        RetentionConfig {
            log_ttl_ms: 30 * DAY_MS,
            artifact_ttl_ms: 20 * DAY_MS,
        },
    )
    .await
    .unwrap();
    assert_eq!(pruned, 0, "leader-gated");

    tdb.cleanup().await;
}

// ---------------------------------------------------------------------------
// Workspace-CAS mark-sweep GC (ADR-0050) — real Postgres + a real local CAS.
// ---------------------------------------------------------------------------

use scarab_server::retention::{sweep_cas, GcConfig};
use scarab_storage::Cas;

async fn seed_run_with_workspace(
    db: &PostgresDb,
    cas: &Arc<dyn Cas>,
    id: &str,
    terminal: bool,
    files: &[(&str, &str)],
) -> String {
    let run = RunId(id.into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    let path = &[
        (RunStatus::Pending, RunStatus::Running),
        if terminal {
            (RunStatus::Running, RunStatus::Succeeded)
        } else {
            (RunStatus::Running, RunStatus::Suspended)
        },
    ];
    for (from, to) in path {
        db.record_transition(&run, *from, *to).await.unwrap();
    }
    db.create_step_run(&run, &StepId("s1".into()), None, &[], Timestamp(0))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        std::fs::write(dir.path().join(name), content).unwrap();
    }
    let root = cas
        .ingest(dir.path().to_str().unwrap())
        .await
        .unwrap()
        .root
        .0;
    db.set_step_output(&run, &StepId("s1".into()), &AttemptId("a1".into()), &root, None)
        .await
        .unwrap();
    root
}

#[tokio::test]
async fn cas_gc_sweeps_only_unreachable_aged_objects() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();

    let cas_dir = tempfile::tempdir().unwrap();
    let storage =
        Arc::new(scarab_storage_s3::S3Storage::local(cas_dir.path().to_str().unwrap()).unwrap());
    let cas: Arc<dyn Cas> = storage.clone();
    let store: Arc<dyn ObjectStore> = storage.clone();

    // Old TERMINAL run: its workspace becomes unreachable. One file is SHARED
    // with the suspended run — the dedup case the mark walk must protect.
    let old_root = seed_run_with_workspace(
        &pg,
        &cas,
        "gc-old-done",
        true,
        &[("only-old.txt", "old"), ("shared.txt", "same-bytes")],
    )
    .await;
    // Old SUSPENDED run: reachable forever, regardless of age.
    let suspended_root = seed_run_with_workspace(
        &pg,
        &cas,
        "gc-old-suspended",
        false,
        &[("keep.txt", "keep"), ("shared.txt", "same-bytes")],
    )
    .await;
    // Age both runs' rows well past the TTL.
    for id in ["gc-old-done", "gc-old-suspended"] {
        sqlx::query("UPDATE runs SET updated_at = 0 WHERE id = $1")
            .bind(id)
            .execute(&tdb.pool)
            .await
            .unwrap();
    }
    // Fresh TERMINAL run: within TTL, reachable.
    let fresh_root =
        seed_run_with_workspace(&pg, &cas, "gc-fresh-done", true, &[("fresh.txt", "fresh")]).await;

    let db: Arc<dyn Db> = Arc::new(pg);
    // Real "now" (+1 min so freshly written CAS files are strictly older than
    // the clock): grace-window arithmetic compares against real file mtimes.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(now_ms));

    // Pass 1 — a HUGE grace window: nothing is swept even though the old
    // terminal run is unreachable (in-flight-ingest protection).
    let swept = sweep_cas(
        &db,
        &cas,
        &store,
        &clock,
        "gc-1",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: i64::MAX,
        },
    )
    .await
    .unwrap();
    assert_eq!(swept, 0, "grace window protects young objects");

    // Pass 2 — no grace: exactly the old terminal run's UNSHARED objects go.
    let swept = sweep_cas(
        &db,
        &cas,
        &store,
        &clock,
        "gc-1",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: 0,
        },
    )
    .await
    .unwrap();
    assert!(swept > 0, "the unreachable workspace was collected");

    // The suspended (never-collectable) and fresh workspaces materialize fine.
    for (root, file) in [(&suspended_root, "keep.txt"), (&fresh_root, "fresh.txt")] {
        let out = tempfile::tempdir().unwrap();
        cas.materialize(
            &scarab_storage::TreeHash(root.clone()),
            out.path().to_str().unwrap(),
        )
        .await
        .expect("reachable workspace survives GC");
        assert!(out.path().join(file).exists());
    }
    // The SHARED blob survived (marked via the suspended run) even though the
    // old run that also referenced it was collected.
    let out = tempfile::tempdir().unwrap();
    cas.materialize(
        &scarab_storage::TreeHash(suspended_root.clone()),
        out.path().to_str().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(out.path().join("shared.txt")).unwrap(),
        "same-bytes"
    );

    // The old root's tree object is gone: materializing it now fails.
    let out = tempfile::tempdir().unwrap();
    assert!(
        cas.materialize(
            &scarab_storage::TreeHash(old_root),
            out.path().to_str().unwrap()
        )
        .await
        .is_err(),
        "the unreachable root was actually swept"
    );

    tdb.cleanup().await;
}

#[tokio::test]
async fn cas_gc_skips_a_dangling_root_instead_of_aborting() {
    // A run whose recorded workspace root is MISSING from the CAS (e.g. the
    // object store was switched and the old blobs were wiped) must not wedge GC
    // forever: the mark walk skips the dangling root and the pass still
    // completes, sweeping / keeping everything else correctly.
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();

    let cas_dir = tempfile::tempdir().unwrap();
    let storage =
        Arc::new(scarab_storage_s3::S3Storage::local(cas_dir.path().to_str().unwrap()).unwrap());
    let cas: Arc<dyn Cas> = storage.clone();
    let store: Arc<dyn ObjectStore> = storage.clone();

    // A reachable (suspended → never-collectable) run with a real workspace.
    let live_root =
        seed_run_with_workspace(&pg, &cas, "gc-live", false, &[("keep.txt", "keep")]).await;

    // A reachable run whose recorded root points at a tree that does NOT exist
    // in the CAS — the dangling reference. `gc_workspace_roots` will return it,
    // so the mark walk hits a missing tree.
    let dangling = RunId("gc-dangling".into());
    pg.create_run(&dangling, 1, 1, Timestamp(0)).await.unwrap();
    for (from, to) in [
        (RunStatus::Pending, RunStatus::Running),
        (RunStatus::Running, RunStatus::Suspended),
    ] {
        pg.record_transition(&dangling, from, to).await.unwrap();
    }
    pg.create_step_run(&dangling, &StepId("s1".into()), None, &[], Timestamp(0))
        .await
        .unwrap();
    // A well-formed hash the store was never asked to hold → NotFound on walk.
    let missing = "0".repeat(64);
    pg.set_step_output(
        &dangling,
        &StepId("s1".into()),
        &AttemptId("a1".into()),
        &missing,
        None,
    )
    .await
    .unwrap();

    let db: Arc<dyn Db> = Arc::new(pg);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(now_ms));

    // The pass must SUCCEED despite the dangling root (before the fix it errored
    // "aborting pass"), and the live workspace must survive.
    sweep_cas(
        &db,
        &cas,
        &store,
        &clock,
        "gc-dangle",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: 0,
        },
    )
    .await
    .expect("a dangling root is skipped, not fatal to the pass");

    let out = tempfile::tempdir().unwrap();
    cas.materialize(
        &scarab_storage::TreeHash(live_root),
        out.path().to_str().unwrap(),
    )
    .await
    .expect("the reachable workspace survives the pass");
    assert!(out.path().join("keep.txt").exists());

    // ...and the pass FORGETS the proven-dead reference, so the dangling root
    // leaves the mark set: the warning fires once for a lost root instead of on
    // every pass forever. Both arms of the mark set must be cleared (ADR-0056),
    // which `gc_workspace_roots` no longer reporting it is what actually proves.
    assert_eq!(
        db.step_output(&dangling, &StepId("s1".into()))
            .await
            .unwrap(),
        None,
        "the dangling snapshot reference is cleared"
    );
    assert!(
        !db.gc_workspace_roots(Timestamp(0))
            .await
            .unwrap()
            .contains(&missing),
        "a forgotten root is not walked again"
    );

    tdb.cleanup().await;
}

// ---------------------------------------------------------------------------
// ADR-0061 s5 — the cold tier's two escape hatches.
// ---------------------------------------------------------------------------

use scarab_server::retention::CasSnapshots;

/// The manual **pin**: "keep this Run's workspaces". A pinned Run's Workspace
/// Snapshots survive a sweep that would otherwise collect them, and the pin is
/// honoured by *marking*, not by filtering the delete list — so the whole
/// transitive tree under a pinned root survives, blobs included. Releasing the
/// pin returns the Run to the ordinary TTL and the very next sweep collects it,
/// which is what proves the pin (and not some other reachability accident) was
/// doing the work.
#[tokio::test]
async fn a_pinned_run_survives_a_sweep_that_would_otherwise_collect_it() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();

    let cas_dir = tempfile::tempdir().unwrap();
    let storage =
        Arc::new(scarab_storage_s3::S3Storage::local(cas_dir.path().to_str().unwrap()).unwrap());
    let cas: Arc<dyn Cas> = storage.clone();
    let store: Arc<dyn ObjectStore> = storage.clone();

    // Two OLD, TERMINAL, otherwise-identical runs — both well past the TTL, so
    // both are collectable. The only difference will be the pin. They share a
    // blob, so this also proves the pin marks a whole tree rather than one object.
    let pinned_root = seed_run_with_workspace(
        &pg,
        &cas,
        "pin-kept",
        true,
        &[("evidence.txt", "the thing being investigated"), ("shared.txt", "same-bytes")],
    )
    .await;
    let unpinned_root = seed_run_with_workspace(
        &pg,
        &cas,
        "pin-none",
        true,
        &[("other.txt", "collectable"), ("shared.txt", "same-bytes")],
    )
    .await;
    for id in ["pin-kept", "pin-none"] {
        sqlx::query("UPDATE runs SET updated_at = 0 WHERE id = $1")
            .bind(id)
            .execute(&tdb.pool)
            .await
            .unwrap();
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(now_ms));

    // Pin one of them — through the engine action, so the audit event is part of
    // what is under test, not just the column.
    let db: Arc<dyn Db> = Arc::new(pg);
    let pinned = RunId("pin-kept".into());
    assert!(
        scarab_engine::pin_run_snapshots(&*db, &*clock, &pinned, Some("alice".into()))
            .await
            .unwrap(),
        "an existing run can be pinned"
    );
    let r = db
        .run_snapshot_retention(&pinned)
        .await
        .unwrap()
        .expect("the run exists");
    assert!(r.terminal, "the pinned run is terminal (on the TTL clock)");
    assert_eq!(
        r.pinned_by.as_deref(),
        Some("alice"),
        "the pin records WHO — an exception that costs storage must be attributable"
    );
    assert!(
        db.events(&pinned).await.unwrap().iter().any(|e| matches!(
            e.kind,
            scarab_engine::EventPayload::RunSnapshotsPinned { .. }
        )),
        "the pin is in the audit log, not only in a column"
    );
    // Pinning is idempotent (re-stamping, never an error).
    assert!(
        scarab_engine::pin_run_snapshots(&*db, &*clock, &pinned, Some("alice".into()))
            .await
            .unwrap()
    );
    assert!(
        !scarab_engine::pin_run_snapshots(&*db, &*clock, &RunId("nope".into()), None)
            .await
            .unwrap(),
        "pinning a run that does not exist reports false, not a phantom pin"
    );

    // Sweep with no grace: the UNPINNED old run goes, the PINNED one stays.
    let swept = sweep_cas(
        &db,
        &cas,
        &store,
        &clock,
        "gc-pin",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: 0,
        },
    )
    .await
    .unwrap();
    assert!(swept > 0, "the unpinned expired workspace was collected");

    let out = tempfile::tempdir().unwrap();
    cas.materialize(
        &scarab_storage::TreeHash(pinned_root.clone()),
        out.path().to_str().unwrap(),
    )
    .await
    .expect("a pinned workspace survives its TTL");
    assert_eq!(
        std::fs::read_to_string(out.path().join("evidence.txt")).unwrap(),
        "the thing being investigated",
        "the pinned tree's BLOBS survive too — the pin marks, it does not post-filter"
    );
    let out = tempfile::tempdir().unwrap();
    assert!(
        cas.materialize(
            &scarab_storage::TreeHash(unpinned_root),
            out.path().to_str().unwrap()
        )
        .await
        .is_err(),
        "the unpinned peer really was collected (so the pin, not luck, kept the other)"
    );

    // Release the pin: the next sweep collects it. This is the half that proves
    // the pin is a live predicate rather than a one-off reprieve.
    assert!(
        scarab_engine::unpin_run_snapshots(&*db, &*clock, &pinned, Some("alice".into()))
            .await
            .unwrap()
    );
    let r = db.run_snapshot_retention(&pinned).await.unwrap().unwrap();
    assert!(r.pinned_at.is_none() && r.pinned_by.is_none(), "the pin is released");
    sweep_cas(
        &db,
        &cas,
        &store,
        &clock,
        "gc-pin",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: 0,
        },
    )
    .await
    .unwrap();
    let out = tempfile::tempdir().unwrap();
    assert!(
        cas.materialize(
            &scarab_storage::TreeHash(pinned_root),
            out.path().to_str().unwrap()
        )
        .await
        .is_err(),
        "an UNpinned run returns to the ordinary TTL and is collected"
    );

    tdb.cleanup().await;
}

/// Seed a `clone → build → test` chain in which every step has a real CAS
/// snapshot, and return the three step ids. The shape that matters: `test`
/// inherits `build`'s workspace, `build` inherits `clone`'s, and `clone`
/// consumes nothing — so widening has somewhere to walk back to.
async fn seed_chain(db: &PostgresDb, cas: &Arc<dyn Cas>, run: &RunId) -> Vec<String> {
    db.create_run(run, 1, 1, Timestamp(0)).await.unwrap();
    for (from, to) in [
        (RunStatus::Pending, RunStatus::Running),
        (RunStatus::Running, RunStatus::Succeeded),
    ] {
        db.record_transition(run, from, to).await.unwrap();
    }
    let chain = ["clone", "build", "test"];
    let mut prev: Option<StepId> = None;
    for name in chain {
        let step = StepId(name.to_string());
        let needs: Vec<StepId> = prev.clone().into_iter().collect();
        db.create_step_run(run, &step, None, &needs, Timestamp(0))
            .await
            .unwrap();
        // A distinct snapshot per step, and the input signature admission would
        // have recorded when the step last ran — the state a rerun starts from.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(format!("{name}.txt")), name).unwrap();
        let snap = cas.ingest(dir.path().to_str().unwrap()).await.unwrap();
        db.set_step_output(
            run,
            &step,
            &AttemptId("a1".into()),
            &snap.root.0,
            snap.identity.as_ref().map(|i| i.0.as_str()),
        )
        .await
        .unwrap();
        db.record_step_transition(run, &step, StepStatus::Pending, StepStatus::Succeeded)
            .await
            .unwrap();
        prev = Some(step);
    }
    // The signatures admission stored. `clone` consumes nothing, so its signature
    // is the empty string — the exact case that makes a naive widening useless,
    // because "unchanged" would then skip the step that has to regenerate.
    let mut output_of = std::collections::HashMap::new();
    for name in chain {
        let step = StepId(name.to_string());
        let needs: Vec<StepId> = db
            .steps_of_run(run)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step == step)
            .unwrap()
            .needs;
        let sig = scarab_engine::input_signature(&needs, None, &output_of);
        db.set_step_input(run, &step, Some(&sig)).await.unwrap();
        // `step_output_identity`, not `step_output` — admission signs each
        // upstream's content IDENTITY, not its snapshot root (ADR-0061 s8). A
        // fixture that signed roots would build a state admission never writes,
        // and the widening assertions below would be testing a fiction.
        if let Some(o) = db.step_output_identity(run, &step).await.unwrap() {
            output_of.insert(step, o);
        }
    }
    chain.iter().map(|s| s.to_string()).collect()
}

/// **Graceful degradation**: rerunning a step whose input Workspace Snapshot the
/// sweeper already collected must *widen* upstream until it reaches something that
/// can regenerate the data — in the limit, `clone` — rather than dispatching a Pod
/// that could never be provisioned. And it must say so: the plan names the widened
/// steps and where the run restarts from, resolved BEFORE anything is re-armed.
#[tokio::test]
async fn an_expired_input_widens_a_rerun_back_to_clone_instead_of_failing() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();

    let cas_dir = tempfile::tempdir().unwrap();
    let storage =
        Arc::new(scarab_storage_s3::S3Storage::local(cas_dir.path().to_str().unwrap()).unwrap());
    let cas: Arc<dyn Cas> = storage.clone();
    let store: Arc<dyn ObjectStore> = storage.clone();

    let run = RunId("widen-1".into());
    seed_chain(&pg, &cas, &run).await;
    // Age the run past its TTL, then let the REAL sweeper collect its snapshots —
    // no hand-deleted objects, so what the rerun sees is exactly what a Run
    // reopened after its retention window sees in production.
    sqlx::query("UPDATE runs SET updated_at = 0 WHERE id = $1")
        .bind(&run.0)
        .execute(&tdb.pool)
        .await
        .unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(now_ms));
    let db: Arc<dyn Db> = Arc::new(pg);
    let swept = sweep_cas(
        &db,
        &cas,
        &store,
        &clock,
        "gc-widen",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: 0,
        },
    )
    .await
    .unwrap();
    assert!(swept > 0, "the expired run's snapshots were collected");

    let oracle = CasSnapshots(cas.clone());
    let snapshots: &dyn scarab_engine::WorkspaceSnapshots = &oracle;
    let test = StepId("test".into());

    // The PREVIEW first: the affordance must be able to say what it is about to
    // do before the user confirms (ADR-0027 — smart never means mysterious).
    let plan = scarab_engine::plan_rerun(&*db, Some(snapshots), &run, &test)
        .await
        .unwrap();
    assert!(plan.is_widened(), "expired inputs widened the rerun");
    assert_eq!(
        plan.invalidated,
        vec![
            StepId("build".into()),
            StepId("clone".into()),
            StepId("test".into())
        ],
        "the whole chain re-runs: the target plus the ancestors that regenerate its inputs"
    );
    assert_eq!(
        plan.widened,
        vec![StepId("build".into()), StepId("clone".into())],
        "the widened subset is exactly the ancestors — reported separately so the copy \
         can distinguish 'rerun this step' from 'this re-runs from clone'"
    );
    assert_eq!(
        plan.starts_from,
        vec![StepId("clone".into())],
        "the run restarts from clone — the phrase the affordance says out loud"
    );
    assert!(
        plan.expired.iter().any(|e| e.consumer == test
            && e.produced_by == StepId("build".into())),
        "the diagnostic names which step's snapshot went missing, not just that one did"
    );

    // Without the oracle the answer is the pre-0061 one: target + descendants
    // only. The contrast is the point — widening comes from proven absence, never
    // from a policy guess.
    let narrow = scarab_engine::plan_rerun(&*db, None, &run, &test)
        .await
        .unwrap();
    assert_eq!(narrow.invalidated, vec![test.clone()]);
    assert!(!narrow.is_widened());

    // Now DO it. Every widened step must be re-armed to Pending *and* have its
    // stored input signature cleared: `clone` re-runs to the same tree hash by
    // construction, so a surviving signature would make admission "skip — inputs
    // unchanged" and carry the dead snapshot forward, i.e. widen and achieve
    // nothing.
    let done = scarab_engine::rerun_step_widened(
        &*db,
        &*clock,
        Some(snapshots),
        &run,
        &test,
        Some("alice".into()),
    )
    .await
    .unwrap();
    assert_eq!(done.widened, plan.widened, "the executed scope is the previewed scope");
    for name in ["clone", "build", "test"] {
        let step = StepId(name.into());
        let s = db
            .steps_of_run(&run)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step == step)
            .unwrap();
        assert_eq!(
            s.status,
            StepStatus::Pending,
            "{name} is re-armed to run again"
        );
        assert_eq!(
            db.step_input(&run, &step).await.unwrap(),
            None,
            "{name}'s input signature is cleared, so admission cannot skip it as unchanged"
        );
    }
    // The widening is recorded on the Take boundary itself, so an audit of "why
    // did rerunning one step re-run the whole pipeline?" is answerable.
    let widened_event = db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .find_map(|e| match e.kind {
            scarab_engine::EventPayload::RunRerunRequested { widened, .. } => Some(widened),
            _ => None,
        })
        .expect("the rerun emitted its Take boundary");
    assert_eq!(
        widened_event,
        vec![StepId("build".into()), StepId("clone".into())],
        "the event records the widened set, not merely the final invalidation set"
    );

    tdb.cleanup().await;
}

/// The same rerun, on a run whose snapshots are still there: nothing widens.
/// Guards the obvious regression — an oracle that reports absence too eagerly
/// would turn every rerun into a full pipeline re-run, which is exactly the
/// waste ADR-0027's cascade rules exist to avoid.
#[tokio::test]
async fn a_rerun_with_live_inputs_stays_narrow() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();
    let cas_dir = tempfile::tempdir().unwrap();
    let storage =
        Arc::new(scarab_storage_s3::S3Storage::local(cas_dir.path().to_str().unwrap()).unwrap());
    let cas: Arc<dyn Cas> = storage.clone();

    let run = RunId("widen-none".into());
    seed_chain(&pg, &cas, &run).await;
    let db: Arc<dyn Db> = Arc::new(pg);
    let oracle = CasSnapshots(cas.clone());
    let snapshots: &dyn scarab_engine::WorkspaceSnapshots = &oracle;

    let plan = scarab_engine::plan_rerun(&*db, Some(snapshots), &run, &StepId("test".into()))
        .await
        .unwrap();
    assert!(!plan.is_widened(), "live inputs widen nothing");
    assert_eq!(plan.invalidated, vec![StepId("test".into())]);
    assert_eq!(plan.starts_from, vec![StepId("test".into())]);
    assert!(plan.expired.is_empty());

    tdb.cleanup().await;
}
