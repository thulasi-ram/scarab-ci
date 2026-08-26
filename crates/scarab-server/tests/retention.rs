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
    let report = sweep_cas(
        &db,
        &cas,
        &store,
        &clock,
        "gc-1",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: i64::MAX,
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(report.swept, 0, "grace window protects young objects");
    // The CLEAN-pass guard for the torn-cold detector's diff DIRECTION
    // (ticket d4d3b95): cold is full of cold-EXTRA objects right now (the old
    // run's unreachable snapshot survives only because of grace), and none of
    // them are residue — residue is marked-minus-cold only. An inverted diff
    // would light up on exactly this fixture.
    assert!(
        report.residue.is_empty() && report.suppressed_residue.is_empty(),
        "nothing is torn: cold-extra objects are sweep candidates, never residue"
    );
    assert_eq!(
        scarab_server::metrics::cas_gc_depot_probe_failed(),
        0,
        "no probe was made (no Depot configured) — the probe-failed gauge holds 0"
    );

    // Pass 2 — no grace: exactly the old terminal run's UNSHARED objects go.
    let report = sweep_cas(
        &db,
        &cas,
        &store,
        &clock,
        "gc-1",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: 0,
        },
        None,
    )
    .await
    .unwrap();
    assert!(report.swept > 0, "the unreachable workspace was collected");
    assert!(
        report.residue.is_empty() && report.suppressed_residue.is_empty(),
        "a pass that sweeps garbage is still not a torn one"
    );

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
        None,
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
            .iter()
            .any(|(root, _)| root == &missing),
        "a forgotten root is not walked again"
    );

    tdb.cleanup().await;
}

// ---------------------------------------------------------------------------
// Torn-cold detection (ticket d4d3b95): the mark walk reads through the TIERED
// handle, so an object only the warm tier still holds marks clean and the
// walk's own torn-CAS error can never fire — cold can silently be missing
// reachable data until the warm volume dies. The sweep's residue diff
// (marked − cold listing) is the detector.
// ---------------------------------------------------------------------------

use scarab_storage::tiered::TieredCas;
use scarab_storage::{TreeHash, TreeTarget};

/// Seed a SUSPENDED (never-collectable, so always-marked) run whose workspace
/// is the directory at `dir`, present in BOTH tiers. `TieredCas::ingest` is a
/// deliberate loud refusal since ADR-0064 (drains write warm then flush), so
/// the both-tiers state is built the content-addressed way: one ingest per
/// tier of the same directory yields byte-identical objects and one root.
async fn seed_live_run_with_dir(
    db: &PostgresDb,
    warm: &Arc<scarab_storage_s3::S3Storage>,
    cold: &Arc<scarab_storage_s3::S3Storage>,
    id: &str,
    dir: &std::path::Path,
) -> String {
    let run = RunId(id.into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    for (from, to) in [
        (RunStatus::Pending, RunStatus::Running),
        (RunStatus::Running, RunStatus::Suspended),
    ] {
        db.record_transition(&run, from, to).await.unwrap();
    }
    db.create_step_run(&run, &StepId("s1".into()), None, &[], Timestamp(0))
        .await
        .unwrap();
    let root = warm.ingest(dir.to_str().unwrap()).await.unwrap().root.0;
    let cold_root = cold.ingest(dir.to_str().unwrap()).await.unwrap().root.0;
    assert_eq!(root, cold_root, "content addressing: same dir, same root");
    db.set_step_output(&run, &StepId("s1".into()), &AttemptId("a1".into()), &root, None)
        .await
        .unwrap();
    root
}

/// The ticket's required test: a REAL torn state — one blob and one inner tree
/// of a reachable snapshot deleted from COLD ONLY, warm intact — must be
/// detected by the sweep, naming both the address and the first root that
/// reaches it; the objects stay marked (never swept) and the root is never
/// forgotten. Mutations killed: remove the residue diff → `report.residue`
/// is empty and nothing detects the tear; break the first-root provenance →
/// the alarm names run A's root (or none) instead of run B's, the only walk
/// that reaches the torn objects.
#[tokio::test]
async fn cas_gc_detects_reachable_objects_missing_from_cold_only() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();

    // The control plane's exact topology: a tiered pair whose warm and cold
    // tiers are separate real stores, reads falling through warm-first.
    let warm_dir = tempfile::tempdir().unwrap();
    let cold_dir = tempfile::tempdir().unwrap();
    let warm =
        Arc::new(scarab_storage_s3::S3Storage::local(warm_dir.path().to_str().unwrap()).unwrap());
    let cold =
        Arc::new(scarab_storage_s3::S3Storage::local(cold_dir.path().to_str().unwrap()).unwrap());
    let tiered: Arc<dyn Cas> =
        Arc::new(TieredCas::new(warm.clone(), cold.clone()).fall_through_on_warm_error());
    let cold_store: Arc<dyn ObjectStore> = cold.clone();

    // Run A: untorn — so provenance has a WRONG root available to name.
    let a_dir = tempfile::tempdir().unwrap();
    std::fs::write(a_dir.path().join("a.txt"), "alpha").unwrap();
    let root_a = seed_live_run_with_dir(&pg, &warm, &cold, "torn-a", a_dir.path()).await;

    // Run B: a top-level blob AND a subdirectory (an inner tree) to tear.
    let b_dir = tempfile::tempdir().unwrap();
    std::fs::write(b_dir.path().join("b.txt"), "beta").unwrap();
    std::fs::create_dir(b_dir.path().join("sub")).unwrap();
    std::fs::write(b_dir.path().join("sub").join("inner.txt"), "inner-beta").unwrap();
    let root_b = seed_live_run_with_dir(&pg, &warm, &cold, "torn-b", b_dir.path()).await;

    // Both roots were recorded LONGER ago than the grace window, so the
    // suppression below must NOT swallow the alarm (`set_step_output` stamps
    // `step_runs.updated_at`, the recording clock `gc_workspace_roots` reports).
    for id in ["torn-a", "torn-b"] {
        sqlx::query("UPDATE step_runs SET updated_at = 0 WHERE run_id = $1")
            .bind(id)
            .execute(&tdb.pool)
            .await
            .unwrap();
    }

    // The torn state: run B's top-level blob and its inner tree vanish from
    // COLD ONLY. Warm keeps them, so the mark walk (and every read) stays
    // green — before the residue diff, this pass reported nothing at all.
    let entries = tiered.tree_entries(&TreeHash(root_b.clone())).await.unwrap();
    let torn_blob = entries
        .iter()
        .find_map(|e| match &e.target {
            TreeTarget::Blob(b) => Some(b.0.clone()),
            _ => None,
        })
        .expect("root B has a top-level blob");
    let torn_tree = entries
        .iter()
        .find_map(|e| match &e.target {
            TreeTarget::Tree(t) => Some(t.0.clone()),
            _ => None,
        })
        .expect("root B has an inner tree");
    cold_store.delete(&format!("blobs/{torn_blob}")).await.unwrap();
    cold_store.delete(&format!("trees/{torn_tree}")).await.unwrap();

    let db: Arc<dyn Db> = Arc::new(pg);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(now_ms));
    let report = sweep_cas(
        &db,
        &tiered,
        &cold_store,
        &clock,
        "gc-torn",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: DAY_MS,
        },
        None,
    )
    .await
    .unwrap();

    // Detection fires, as data: exactly the two torn addresses, each carrying
    // the root whose walk reached it.
    let mut expected = vec![format!("blobs/{torn_blob}"), format!("trees/{torn_tree}")];
    expected.sort();
    let keys: Vec<String> = report.residue.iter().map(|r| r.key.clone()).collect();
    assert_eq!(keys, expected, "exactly the two torn objects are residue");
    for r in &report.residue {
        assert_eq!(
            r.root, root_b,
            "provenance names the root whose walk reaches the object — run B's, never run A's"
        );
        assert_ne!(r.root, root_a);
        assert_eq!(
            r.root_recorded_at,
            Timestamp(0),
            "the entry carries the recording clock the suppression compared against"
        );
    }
    assert!(
        report.suppressed_residue.is_empty(),
        "a root older than grace is alarmed, not suppressed"
    );
    assert_eq!(report.swept, 0, "everything is reachable — detection deletes nothing");
    // The operator-visible counter: gauge-like, SET by this pass.
    assert_eq!(scarab_server::metrics::cas_gc_cold_residue(), 2);
    assert_eq!(scarab_server::metrics::cas_gc_cold_residue_suppressed(), 0);

    // The residue objects are MARKED, so the sweep cannot have deleted
    // anything of run B's; warm still holds the torn bytes; and the
    // dangling-root self-heal did NOT fire — the root read fine (through
    // warm), so it was never proven dead.
    assert!(
        warm.get(&format!("blobs/{torn_blob}")).await.is_ok(),
        "warm still holds the torn blob (the recovery source the follow-up will use)"
    );
    assert!(
        warm.get(&format!("trees/{torn_tree}")).await.is_ok(),
        "warm still holds the torn tree"
    );
    assert!(
        cold_store.get(&format!("trees/{root_b}")).await.is_ok(),
        "the marked root object survives in cold"
    );
    assert_eq!(
        db.step_output(&RunId("torn-b".into()), &StepId("s1".into()))
            .await
            .unwrap(),
        Some(root_b.clone()),
        "the root is NOT forgotten — forget is only for roots absent from BOTH tiers"
    );
    assert!(
        db.gc_workspace_roots(Timestamp(0))
            .await
            .unwrap()
            .iter()
            .any(|(root, _)| root == &root_b),
        "the root stays in the mark set, so the next pass re-detects until repaired"
    );

    tdb.cleanup().await;
}

/// Suppression: the SAME torn state under a root recorded moments ago must not
/// alarm — under ADR-0064 its cold flush may still be in flight — but the hole
/// is still counted (report + gauge), just at debug level. Mutation killed:
/// drop the age check → the fresh root's residue lands in `residue` and the
/// alarmed gauge goes non-zero, a false alarm on every settle.
#[tokio::test]
async fn residue_under_a_fresh_root_is_suppressed_not_alarmed() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();

    let warm_dir = tempfile::tempdir().unwrap();
    let cold_dir = tempfile::tempdir().unwrap();
    let warm =
        Arc::new(scarab_storage_s3::S3Storage::local(warm_dir.path().to_str().unwrap()).unwrap());
    let cold =
        Arc::new(scarab_storage_s3::S3Storage::local(cold_dir.path().to_str().unwrap()).unwrap());
    let tiered: Arc<dyn Cas> =
        Arc::new(TieredCas::new(warm.clone(), cold.clone()).fall_through_on_warm_error());
    let cold_store: Arc<dyn ObjectStore> = cold.clone();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fresh.txt"), "just-settled").unwrap();
    let root = seed_live_run_with_dir(&pg, &warm, &cold, "torn-fresh", dir.path()).await;
    // Deliberately NOT aged: `set_step_output` stamped `step_runs.updated_at`
    // with wall-clock now, so the root's recording sits INSIDE the grace
    // window the sweeper already has (no new knob).

    let entries = tiered.tree_entries(&TreeHash(root.clone())).await.unwrap();
    let torn_blob = entries
        .iter()
        .find_map(|e| match &e.target {
            TreeTarget::Blob(b) => Some(b.0.clone()),
            _ => None,
        })
        .unwrap();
    cold_store.delete(&format!("blobs/{torn_blob}")).await.unwrap();

    let db: Arc<dyn Db> = Arc::new(pg);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(now_ms));
    let report = sweep_cas(
        &db,
        &tiered,
        &cold_store,
        &clock,
        "gc-torn-fresh",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: DAY_MS,
        },
        None,
    )
    .await
    .unwrap();

    assert!(
        report.residue.is_empty(),
        "no error-level alarm while the flush may still be in flight"
    );
    let keys: Vec<String> = report
        .suppressed_residue
        .iter()
        .map(|r| r.key.clone())
        .collect();
    assert_eq!(
        keys,
        vec![format!("blobs/{torn_blob}")],
        "the hole is still COUNTED — suppressed, not invisible"
    );
    assert_eq!(report.suppressed_residue[0].root, root);
    assert_eq!(scarab_server::metrics::cas_gc_cold_residue(), 0);
    assert_eq!(scarab_server::metrics::cas_gc_cold_residue_suppressed(), 1);

    tdb.cleanup().await;
}

/// Ticket 231040a: the residue gauges are leader-reported, so a replica whose
/// pass does NOT hold the lease must ZERO them — otherwise a replica that
/// LOST the lease exports its last leader-era residue forever, a phantom tear
/// to whoever scrapes it. `scarab_cas_gc_leader` (1/0 per pass) tells a scrape
/// whose numbers are live. Mutations killed: remove the non-leader zeroing →
/// the stale non-zero residue survives the second sweep; remove either leader
/// gauge write → the 1-after-leader / 0-after-non-leader assertions fail.
/// (Relies on nextest process-per-test isolation, like every gauge test here.)
#[tokio::test]
async fn residue_gauges_are_zeroed_on_a_non_leader_pass() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();

    let warm_dir = tempfile::tempdir().unwrap();
    let cold_dir = tempfile::tempdir().unwrap();
    let warm =
        Arc::new(scarab_storage_s3::S3Storage::local(warm_dir.path().to_str().unwrap()).unwrap());
    let cold =
        Arc::new(scarab_storage_s3::S3Storage::local(cold_dir.path().to_str().unwrap()).unwrap());
    let tiered: Arc<dyn Cas> =
        Arc::new(TieredCas::new(warm.clone(), cold.clone()).fall_through_on_warm_error());
    let cold_store: Arc<dyn ObjectStore> = cold.clone();

    // A real torn state under an aged root, so the LEADER pass reports
    // genuinely non-zero residue for the non-leader pass to phantom-export.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("t.txt"), "torn-for-gauges").unwrap();
    let root = seed_live_run_with_dir(&pg, &warm, &cold, "torn-gauge", dir.path()).await;
    sqlx::query("UPDATE step_runs SET updated_at = 0 WHERE run_id = $1")
        .bind("torn-gauge")
        .execute(&tdb.pool)
        .await
        .unwrap();
    let entries = tiered.tree_entries(&TreeHash(root.clone())).await.unwrap();
    let torn_blob = entries
        .iter()
        .find_map(|e| match &e.target {
            TreeTarget::Blob(b) => Some(b.0.clone()),
            _ => None,
        })
        .unwrap();
    cold_store.delete(&format!("blobs/{torn_blob}")).await.unwrap();

    let db: Arc<dyn Db> = Arc::new(pg);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(now_ms));
    let cfg = GcConfig {
        workspace_ttl_ms: 30 * DAY_MS,
        grace_ms: DAY_MS,
    };

    // Leader pass: real residue, and the leader gauge says these numbers live.
    let report = sweep_cas(&db, &tiered, &cold_store, &clock, "gc-gauge-leader", cfg, None)
        .await
        .unwrap();
    assert_eq!(report.residue.len(), 1, "the leader pass detects the tear");
    assert_eq!(scarab_server::metrics::cas_gc_cold_residue(), 1);
    assert_eq!(scarab_server::metrics::cas_gc_leader(), 1);

    // A second sweeper id while the lease is held (the ADR-0050 trick): its
    // pass is NOT the leader, asserts nothing, and must zero the gauges.
    let report = sweep_cas(&db, &tiered, &cold_store, &clock, "gc-gauge-follower", cfg, None)
        .await
        .unwrap();
    assert!(report.residue.is_empty(), "a non-leader reports nothing");
    assert_eq!(
        scarab_server::metrics::cas_gc_cold_residue(),
        0,
        "a non-leader pass zeroes the alarmed gauge — no phantom tear"
    );
    assert_eq!(
        scarab_server::metrics::cas_gc_cold_residue_suppressed(),
        0,
        "…and the suppressed gauge"
    );
    assert_eq!(
        scarab_server::metrics::cas_gc_leader(),
        0,
        "the leader gauge says this replica's numbers are not live"
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
    let report = sweep_cas(
        &db,
        &cas,
        &store,
        &clock,
        "gc-pin",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: 0,
        },
        None,
    )
    .await
    .unwrap();
    assert!(report.swept > 0, "the unpinned expired workspace was collected");

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
        None,
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
    // is the bare `v2:` prefix — the exact case that makes a naive widening useless,
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
    let report = sweep_cas(
        &db,
        &cas,
        &store,
        &clock,
        "gc-widen",
        GcConfig {
            workspace_ttl_ms: 30 * DAY_MS,
            grace_ms: 0,
        },
        None,
    )
    .await
    .unwrap();
    assert!(report.swept > 0, "the expired run's snapshots were collected");

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

// ---------------------------------------------------------------------------
// The retention PROMISE the run resource states (ADR-0061 s5) — real Postgres.
// ---------------------------------------------------------------------------

/// `expires_at` is `updated_at + window`, and `updated_at` is a column only the
/// real adapter has.
///
/// `snapshots_pin_api.rs` asserts the same arithmetic, but over `InMemoryDb`,
/// whose `run_snapshot_retention` reports **creation** time. There
/// `created_at == updated_at` by construction, so that assertion cannot tell
/// which column the promise is keyed on — and `updated_at` is the one the GC
/// sweeper's cutoff compares against. A run created on Monday and settled on
/// Friday has two very different answers, so this fixture forces the two columns
/// a hundred days apart and then asks the real HTTP surface.
///
/// Three shapes, because the promise is as much about when it is *withheld*:
/// terminal (on the clock), non-terminal at any age (never on it, ADR-0050), and
/// pinned (held open indefinitely).
#[tokio::test]
async fn the_retention_promise_is_keyed_on_updated_at_not_created_at() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // oneshot

    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();

    const CREATED: i64 = 0;
    const SETTLED: i64 = 100 * DAY_MS;
    const WINDOW_DAYS: u32 = 7;

    for (id, terminal) in [
        ("promise-terminal", true),
        ("promise-open", false),
        ("promise-pinned", true),
    ] {
        let run = RunId(id.into());
        pg.create_run(&run, 1, 1, Timestamp(CREATED)).await.unwrap();
        pg.record_transition(&run, RunStatus::Pending, RunStatus::Running)
            .await
            .unwrap();
        pg.record_transition(
            &run,
            RunStatus::Running,
            if terminal {
                RunStatus::Succeeded
            } else {
                RunStatus::Suspended
            },
        )
        .await
        .unwrap();
        pg.create_step_run(&run, &StepId("s1".into()), None, &[], Timestamp(CREATED))
            .await
            .unwrap();
        // `record_transition` stamps `updated_at` with wall-clock now, which is
        // neither reproducible nor far enough from `created_at` to be a
        // discriminating fixture. Pin it to a known settle instant instead.
        sqlx::query("UPDATE runs SET updated_at = $2 WHERE id = $1")
            .bind(&run.0)
            .bind(SETTLED)
            .execute(&tdb.pool)
            .await
            .unwrap();
    }
    // Pinned AFTER the settle, and deliberately not at the settle instant: a pin
    // must not re-date the run.
    pg.pin_run_snapshots(
        &RunId("promise-pinned".into()),
        Some("alice"),
        Timestamp(SETTLED + DAY_MS),
    )
    .await
    .unwrap();

    // The fixture's whole point, asserted rather than assumed: if these two ever
    // coincide, every assertion below stops distinguishing the two columns and
    // this test degenerates into the in-memory one.
    let (created, updated): (i64, i64) = sqlx::query_as(
        "SELECT created_at, updated_at FROM runs WHERE id = 'promise-terminal'",
    )
    .fetch_one(&tdb.pool)
    .await
    .unwrap();
    assert_eq!(created, CREATED);
    assert_eq!(updated, SETTLED);
    assert_ne!(created, updated, "the fixture must keep the two columns apart");

    // Two days after settling: inside a 7-day window.
    let now = SETTLED + 2 * DAY_MS;
    let db: Arc<dyn Db> = Arc::new(pg);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(now));
    let logs = Arc::new(scarab_server::LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = scarab_server::router(
        scarab_server::AppState::new(db, clock, logs)
            .with_snapshot_retention_days(WINDOW_DAYS),
    );

    let retention_of = |id: &'static str| {
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/v1/runs/{id}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{id}");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["snapshot_retention"]
                .clone()
        }
    };

    // 1. Terminal, unpinned: on the clock, measured from `updated_at`.
    let wr = retention_of("promise-terminal").await;
    assert_eq!(wr["retention_days"], WINDOW_DAYS);
    assert_eq!(
        wr["expires_at"],
        SETTLED + (WINDOW_DAYS as i64) * DAY_MS,
        "the promise is settled_at + window, and settled_at is the `updated_at` \
         column the sweeper keys on — NOT `created_at`, which would answer {}",
        CREATED + (WINDOW_DAYS as i64) * DAY_MS
    );
    assert_eq!(wr["expired"], false);
    assert_eq!(wr["pinned"], false);

    // 2. Non-terminal, aged well past any window: no expiry is promised at all.
    sqlx::query("UPDATE runs SET updated_at = 0 WHERE id = 'promise-open'")
        .execute(&tdb.pool)
        .await
        .unwrap();
    let wr = retention_of("promise-open").await;
    assert!(
        wr.get("expires_at").is_none(),
        "a run that has not settled is never GC-eligible, at any age (ADR-0050), \
         so quoting a date for it would be a lie the sweeper does not tell: {wr}"
    );
    assert_eq!(wr["expired"], false);

    // 3. Pinned: the window is held open, so there is no date to quote.
    let wr = retention_of("promise-pinned").await;
    assert!(
        wr.get("expires_at").is_none(),
        "a pin holds the window open indefinitely: {wr}"
    );
    assert_eq!(wr["pinned"], true);
    assert_eq!(wr["pinned_by"], "alice");
    assert_eq!(wr["pinned_at"], SETTLED + DAY_MS);
    assert_eq!(wr["expired"], false);

    tdb.cleanup().await;
}

// ---------------------------------------------------------------------------
// Pack-durable subtraction in the torn-cold detector (ADR-0067).
// ---------------------------------------------------------------------------

use scarab_server::retention::DepotDurableIndex;
use std::collections::HashSet;

/// A real-shaped durable-index probe: answers that EVERYTHING asked about is
/// durable in packs (nothing missing) — the post-ADR-0067 steady state, where
/// durable drains write packs and the loose listing vouches for nothing.
struct AllDurable;

#[async_trait::async_trait]
impl DepotDurableIndex for AllDurable {
    async fn durable_missing(
        &self,
        _blobs: Vec<String>,
        _trees: Vec<String>,
    ) -> Result<(HashSet<String>, HashSet<String>), String> {
        Ok((HashSet::new(), HashSet::new()))
    }
}

/// A probe that says NOTHING is durable in packs — legacy loose-only content.
struct NoneDurable;

#[async_trait::async_trait]
impl DepotDurableIndex for NoneDurable {
    async fn durable_missing(
        &self,
        blobs: Vec<String>,
        trees: Vec<String>,
    ) -> Result<(HashSet<String>, HashSet<String>), String> {
        Ok((blobs.into_iter().collect(), trees.into_iter().collect()))
    }
}

/// The probe when the Depot is unreachable or broken.
struct BrokenIndex;

#[async_trait::async_trait]
impl DepotDurableIndex for BrokenIndex {
    async fn durable_missing(
        &self,
        _blobs: Vec<String>,
        _trees: Vec<String>,
    ) -> Result<(HashSet<String>, HashSet<String>), String> {
        Err("POST /v1/cas/have: connection refused".into())
    }
}

/// Build the standard torn fixture (one blob of a reachable, aged root deleted
/// from COLD only) and return everything a sweep needs. Factored so the probe
/// tests below drive the IDENTICAL tear and differ only in the probe's answer.
async fn torn_fixture(
    tdb: &common::TestDb,
    run_id: &str,
) -> (
    Arc<dyn Db>,
    Arc<dyn Cas>,
    Arc<dyn ObjectStore>,
    Arc<dyn Clock>,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();

    let warm_dir = tempfile::tempdir().unwrap();
    let cold_dir = tempfile::tempdir().unwrap();
    let warm =
        Arc::new(scarab_storage_s3::S3Storage::local(warm_dir.path().to_str().unwrap()).unwrap());
    let cold =
        Arc::new(scarab_storage_s3::S3Storage::local(cold_dir.path().to_str().unwrap()).unwrap());
    let tiered: Arc<dyn Cas> =
        Arc::new(TieredCas::new(warm.clone(), cold.clone()).fall_through_on_warm_error());
    let cold_store: Arc<dyn ObjectStore> = cold.clone();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("t.txt"), "torn-under-a-tier-probe").unwrap();
    let root = seed_live_run_with_dir(&pg, &warm, &cold, run_id, dir.path()).await;
    // Aged past the grace window, so nothing suppresses on freshness grounds.
    sqlx::query("UPDATE step_runs SET updated_at = 0 WHERE run_id = $1")
        .bind(run_id)
        .execute(&tdb.pool)
        .await
        .unwrap();
    let entries = tiered.tree_entries(&TreeHash(root.clone())).await.unwrap();
    let torn_blob = entries
        .iter()
        .find_map(|e| match &e.target {
            TreeTarget::Blob(b) => Some(b.0.clone()),
            _ => None,
        })
        .unwrap();
    cold_store.delete(&format!("blobs/{torn_blob}")).await.unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(now_ms));
    (Arc::new(pg), tiered, cold_store, clock, warm_dir, cold_dir)
}

/// A marked object the pack index holds durable is NOT torn-cold residue:
/// durable drains write packs, not loose objects, so the loose cold listing
/// alone would flag every packed object on every pass — a permanent false
/// alarm. And the subtraction is the INDEX's answer, not a blanket suppression:
/// the same tear under a nothing-is-packed answer still alarms.
///
/// Mutations killed: dropping the durable-index subtraction (the AllDurable
/// pass alarms — the control pass proves the fixture is genuinely torn);
/// inverting the missing-set test (the NoneDurable pass goes quiet);
/// suppressing wholesale on a configured probe (the NoneDurable pass again).
#[tokio::test]
async fn a_pack_durable_object_is_not_torn_cold_residue() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let (db, tiered, cold_store, clock, _warm, _cold) = torn_fixture(&tdb, "torn-pack").await;
    let cfg = GcConfig {
        workspace_ttl_ms: 30 * DAY_MS,
        grace_ms: DAY_MS,
    };

    // Control pass, no probe (the no-Depot sweeper shape): the fixture IS
    // torn and the detector, on the loose listing alone, says so.
    let report = sweep_cas(&db, &tiered, &cold_store, &clock, "gc-pack", cfg, None)
        .await
        .unwrap();
    assert_eq!(report.residue.len(), 1, "the fixture is genuinely torn");
    assert_eq!(scarab_server::metrics::cas_gc_cold_residue(), 1);

    // The pack-durable pass: same tear, but the index vouches for the hole.
    // Seed the probe-failed gauge to 1 first, so its 0 below proves the pass
    // SET it per-pass rather than never touching it.
    scarab_server::metrics::set_cas_gc_depot_probe_failed(true);
    let probe = AllDurable;
    let report = sweep_cas(
        &db,
        &tiered,
        &cold_store,
        &clock,
        "gc-pack",
        cfg,
        Some(&probe as &dyn DepotDurableIndex),
    )
    .await
    .unwrap();
    assert!(
        report.residue.is_empty(),
        "a pack-durable object is durable — absence from the LOOSE listing proves nothing"
    );
    assert!(
        report.suppressed_residue.is_empty(),
        "subtracted means SUBTRACTED — not rerouted into the freshness-suppressed bucket"
    );
    assert_eq!(
        scarab_server::metrics::cas_gc_cold_residue(),
        0,
        "the residue gauge is zeroed for the pass, clearing the control pass's 1"
    );
    assert_eq!(scarab_server::metrics::cas_gc_cold_residue_suppressed(), 0);
    assert_eq!(
        scarab_server::metrics::cas_gc_leader(),
        1,
        "this was the LEADER's pass — subtraction must not masquerade as lease loss"
    );
    assert_eq!(
        scarab_server::metrics::cas_gc_depot_probe_failed(),
        0,
        "the probe SUCCEEDED — a subtracted alarm must not read as a broken probe"
    );

    // A nothing-is-packed answer keeps the alarm: the subtraction is per
    // address, never a blanket suppression for having a Depot at all.
    scarab_server::metrics::set_cas_gc_depot_probe_failed(true);
    let probe = NoneDurable;
    let report = sweep_cas(
        &db,
        &tiered,
        &cold_store,
        &clock,
        "gc-pack",
        cfg,
        Some(&probe as &dyn DepotDurableIndex),
    )
    .await
    .unwrap();
    assert_eq!(
        report.residue.len(),
        1,
        "content in no pack and no loose object is REALLY missing — the tear is reported"
    );
    assert_eq!(
        scarab_server::metrics::cas_gc_depot_probe_failed(),
        0,
        "a successful probe reads 0 — this residue is trustworthy"
    );

    tdb.cleanup().await;
}

/// RULING (preserved from the tier-probe era): a durable-index probe ERROR
/// does not suppress — the detector stays on and the failure is only logged.
/// Otherwise a torn cold would be maskable by breaking the Depot, and the one
/// detector that matters would fail exactly when the Depot is failing.
/// Mutation killed: treating `Err` like "everything is packed" → the torn
/// fixture stops alarming here. And the pass must SAY the probe failed:
/// `scarab_cas_gc_depot_probe_failed` reads 1, so a scrape can tell "probe
/// down, residue may be pack-durable false positives" from real torn cold.
#[tokio::test]
async fn a_durable_index_probe_error_does_not_suppress_the_detector() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let (db, tiered, cold_store, clock, _warm, _cold) = torn_fixture(&tdb, "torn-err").await;
    let cfg = GcConfig {
        workspace_ttl_ms: 30 * DAY_MS,
        grace_ms: DAY_MS,
    };

    let report = sweep_cas(
        &db,
        &tiered,
        &cold_store,
        &clock,
        "gc-err",
        cfg,
        Some(&BrokenIndex as &dyn DepotDurableIndex),
    )
    .await
    .unwrap();
    assert_eq!(
        report.residue.len(),
        1,
        "an unreachable Depot is not evidence the content is durable — the tear alarms"
    );
    assert_eq!(scarab_server::metrics::cas_gc_cold_residue(), 1);
    assert_eq!(scarab_server::metrics::cas_gc_leader(), 1);
    assert_eq!(
        scarab_server::metrics::cas_gc_depot_probe_failed(),
        1,
        "the probe-failed gauge distinguishes 'index probe failed' from real torn cold: \
         when 1, the residue above may be pack-durable false positives — fix the probe first"
    );

    tdb.cleanup().await;
}
