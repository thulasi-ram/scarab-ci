//! Amendment F1 of git-bug 4afaa3e: a Depot outage must NEVER be laundered
//! into "expired". The widening oracle (`CasSnapshots`) answers over a tiered
//! handle WITHOUT `fall_through_on_warm_error`, so a warm-leg error propagates
//! into its "a transient error is not proof of expiry" guard (assume present,
//! widen nothing) — instead of falling through to a loose-only cold read that
//! answers a definitive NotFound for content that lives in packs.
//!
//! Two tests: the strict handle keeps the plan narrow through an outage, and
//! the fall-through handle — the Browse/GC composition — demonstrably WOULD
//! have widened, which is exactly why the oracle must not share it.

use std::sync::Arc;

use async_trait::async_trait;
use scarab_engine::{plan_rerun, AttemptId, Db, RunId, StepId, Timestamp, WorkspaceSnapshots};
use scarab_server::retention::CasSnapshots;
use scarab_storage::tiered::TieredCas;
use scarab_storage::{BlobHash, Cas, Snapshot, StorageError, TreeEntry, TreeHash};
use scarab_testkit::InMemoryDb;

/// What the HTTP adapter produces during a Depot outage: `Backend` on every
/// verb — deliberately never `NotFound`, so an unreachable service cannot read
/// as an empty one.
struct DepotDown;

#[async_trait]
impl Cas for DepotDown {
    async fn put_blob(&self, _: &[u8]) -> Result<BlobHash, StorageError> {
        Err(StorageError::Backend("depot unreachable".into()))
    }
    async fn get_blob(&self, _: &BlobHash) -> Result<Vec<u8>, StorageError> {
        Err(StorageError::Backend("depot unreachable".into()))
    }
    async fn put_tree(&self, _: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
        Err(StorageError::Backend("depot unreachable".into()))
    }
    async fn tree_entries(&self, _: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
        Err(StorageError::Backend("depot unreachable".into()))
    }
    async fn materialize(&self, _: &TreeHash, _: &str) -> Result<(), StorageError> {
        Err(StorageError::Backend("depot unreachable".into()))
    }
    async fn ingest(&self, _: &str) -> Result<Snapshot, StorageError> {
        Err(StorageError::Backend("depot unreachable".into()))
    }
}

/// A run whose upstream output snapshot exists only where the (down) Depot can
/// answer for it — the cold leg alone knows nothing (packed content).
async fn seed(db: &InMemoryDb, run: &RunId) {
    db.create_run(run, 1, 1, Timestamp(0)).await.unwrap();
    for (id, needs, root) in [
        ("build", vec![], "packed-root-build"),
        ("test", vec!["build"], "packed-root-test"),
    ] {
        let sid = StepId(id.into());
        let needs: Vec<StepId> = needs.into_iter().map(|n: &str| StepId(n.into())).collect();
        db.create_step_run(run, &sid, None, &needs, Timestamp(0))
            .await
            .unwrap();
        db.set_step_output(run, &sid, &AttemptId("a1".into()), root, Some(root))
            .await
            .unwrap();
    }
}

/// An empty local store standing in for the cold object store's LOOSE objects:
/// it holds nothing, because the content lives in packs only the Depot indexes.
/// The `TempDir` guard rides along so the directory outlives the store.
fn empty_cold() -> (Arc<dyn Cas>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cold = scarab_storage_s3::S3Storage::local(dir.path().to_str().unwrap()).unwrap();
    (Arc::new(cold), dir)
}

/// The strict handle (what `snapshot_oracle` now answers over): the warm-leg
/// error PROPAGATES, `CasSnapshots` assumes present, and the plan stays narrow.
/// An outage makes the preview conservative, never wrong.
#[tokio::test]
async fn a_depot_outage_never_widens_the_plan_over_the_strict_handle() {
    let db = InMemoryDb::new();
    let run = RunId("outage-strict".into());
    seed(&db, &run).await;

    let (cold, _guard) = empty_cold();
    let strict: Arc<dyn Cas> = Arc::new(TieredCas::new(Arc::new(DepotDown), cold));
    let oracle = CasSnapshots(strict);
    let snapshots: &dyn WorkspaceSnapshots = &oracle;

    let plan = plan_rerun(&db as &dyn Db, Some(snapshots), &run, &StepId("test".into()))
        .await
        .unwrap();
    assert!(
        !plan.is_widened(),
        "a warm-leg ERROR is not proof of expiry — the plan must not widen"
    );
    assert_eq!(plan.invalidated, vec![StepId("test".into())]);
    assert!(plan.expired.is_empty());
}

/// The counterfactual that motivated the amendment: the SAME outage over the
/// Browse/GC fall-through handle lands on the loose-only cold leg, which
/// answers a definitive NotFound for packed content — and the plan widens back
/// to `build` on nothing but a restart. This pins WHY the oracle gets its own
/// handle; if this test ever fails, fall-through stopped laundering and the
/// two handles could be re-unified.
#[tokio::test]
async fn the_fall_through_handle_would_launder_the_same_outage_into_expired() {
    let db = InMemoryDb::new();
    let run = RunId("outage-launder".into());
    seed(&db, &run).await;

    let (cold, _guard) = empty_cold();
    let fall_through: Arc<dyn Cas> =
        Arc::new(TieredCas::new(Arc::new(DepotDown), cold).fall_through_on_warm_error());
    let oracle = CasSnapshots(fall_through);
    let snapshots: &dyn WorkspaceSnapshots = &oracle;

    let plan = plan_rerun(&db as &dyn Db, Some(snapshots), &run, &StepId("test".into()))
        .await
        .unwrap();
    assert!(
        plan.is_widened(),
        "documented hazard: fall-through turns the outage into a definitive absence"
    );
    assert_eq!(
        plan.widened,
        vec![StepId("build".into())],
        "…and drags the producer in — the false 'expired' the strict handle prevents"
    );
}
