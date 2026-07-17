//! The retention sweeper (ADR-0050): a leader-gated loop that prunes expired
//! **log** blobs + their Postgres index by class TTL. Eligibility is keyed on
//! run *lifecycle*, never wall-clock alone — the Db port only ever returns
//! TERMINAL runs past the cutoff, so a run suspended on a gate for weeks is
//! untouchable regardless of age. Run metadata (state row, event log) is
//! retained for audit even after its blobs are pruned.
//!
//! Deletion order is bodies-then-index (at-least-once): a crash between the
//! two leaves the index behind, and the next sweep retries. Artifacts join
//! this sweeper when the artifact store lands (ADR-0052).

use std::sync::Arc;

use scarab_engine::{Clock, Db, RunId, Timestamp};
use scarab_storage::ObjectStore;

/// The per-class TTLs (ADR-0030 defaults; env-overridable, ADR-0048).
#[derive(Debug, Clone, Copy)]
pub struct RetentionConfig {
    /// How long a terminal run's LOGS are kept, in milliseconds.
    pub log_ttl_ms: i64,
}

/// The lease name gating the sweeper to one replica at a time.
const RETENTION_LEASE: &str = "retention-sweeper";
const RETENTION_LEASE_TTL_MS: i64 = 5 * 60 * 1000;
/// Runs pruned per sweep pass — bounds one pass's work; the loop catches up.
const SWEEP_BATCH: u32 = 100;

/// One sweep pass. Returns the number of runs whose logs were pruned.
/// A non-leader replica returns 0 without touching anything.
pub async fn sweep_retention(
    db: &Arc<dyn Db>,
    store: &Arc<dyn ObjectStore>,
    clock: &Arc<dyn scarab_engine::Clock>,
    owner: &str,
    cfg: RetentionConfig,
) -> Result<u32, String> {
    // Leader gate (like admission): only the lease holder sweeps.
    let lease = db
        .lease(RETENTION_LEASE, owner, RETENTION_LEASE_TTL_MS)
        .await
        .map_err(|e| e.to_string())?;
    if lease.owner != owner {
        return Ok(0);
    }

    let now = clock.now().await;
    let cutoff = Timestamp(now.0 - cfg.log_ttl_ms);
    let runs = db
        .prunable_log_runs(cutoff, SWEEP_BATCH)
        .await
        .map_err(|e| e.to_string())?;
    let mut pruned = 0u32;
    for run in runs {
        prune_run_logs(db, store, &run).await?;
        pruned += 1;
    }
    Ok(pruned)
}

/// Delete one run's log blobs, then its index. Body deletion is best-effort
/// per key (a missing blob is already gone); the index drop only happens
/// after the pass over the bodies, so a partial failure re-sweeps.
async fn prune_run_logs(
    db: &Arc<dyn Db>,
    store: &Arc<dyn ObjectStore>,
    run: &RunId,
) -> Result<(), String> {
    let keys = db.log_object_keys_of_run(run).await.map_err(|e| e.to_string())?;
    for key in &keys {
        if let Err(e) = store.delete(key).await {
            tracing::warn!(run = %run.0, key = %key, error = %e, "retention: blob delete failed (will re-sweep)");
            return Err(format!("delete {key}: {e}"));
        }
    }
    db.delete_log_index_of_run(run).await.map_err(|e| e.to_string())?;
    tracing::info!(run = %run.0, blobs = keys.len(), "retention: pruned run logs (metadata retained)");
    Ok(())
}

/// Spawn the background sweeper: one pass every `interval`, forever.
pub fn spawn_sweeper(
    db: Arc<dyn Db>,
    store: Arc<dyn ObjectStore>,
    clock: Arc<dyn Clock>,
    owner: String,
    cfg: RetentionConfig,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match sweep_retention(&db, &store, &clock, &owner, cfg).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(runs = n, "retention sweep pass complete"),
                Err(e) => tracing::warn!(error = %e, "retention sweep pass failed"),
            }
            tokio::time::sleep(interval).await;
        }
    })
}
