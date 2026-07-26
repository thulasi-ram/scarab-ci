//! Retention sweep (ADR-0050) against *real* Postgres: only TERMINAL runs
//! past the TTL are prunable; a gate-suspended run is never eligible
//! regardless of age; blobs go first, index second, run metadata survives.
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{AttemptId, Clock, Db, LogChunkMeta, RunId, RunStatus, StepId, Timestamp};
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
    db.set_step_output(&run, &StepId("s1".into()), &AttemptId("a1".into()), &root)
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
