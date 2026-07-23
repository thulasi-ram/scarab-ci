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
    /// How long a terminal run's ARTIFACTS are kept, in milliseconds
    /// (ADR-0052: independent class, ~90d default).
    pub artifact_ttl_ms: i64,
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

    // The artifact class (ADR-0052): same lifecycle key, its own TTL.
    let cutoff = Timestamp(now.0 - cfg.artifact_ttl_ms);
    let runs = db
        .prunable_artifact_runs(cutoff, SWEEP_BATCH)
        .await
        .map_err(|e| e.to_string())?;
    for run in runs {
        let artifacts = db.artifacts_of_run(&run).await.map_err(|e| e.to_string())?;
        for a in &artifacts {
            if let Err(e) = store.delete(&a.meta.object_key).await {
                return Err(format!("delete {}: {e}", a.meta.object_key));
            }
        }
        db.delete_artifacts_of_run(&run)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(run = %run.0, artifacts = artifacts.len(), "retention: pruned run artifacts");
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
    let keys = db
        .log_object_keys_of_run(run)
        .await
        .map_err(|e| e.to_string())?;
    for key in &keys {
        if let Err(e) = store.delete(key).await {
            tracing::warn!(run = %run.0, key = %key, error = %e, "retention: blob delete failed (will re-sweep)");
            return Err(format!("delete {key}: {e}"));
        }
    }
    db.delete_log_index_of_run(run)
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(run = %run.0, blobs = keys.len(), "retention: pruned run logs (metadata retained)");
    Ok(())
}

/// The workspace-CAS GC configuration (ADR-0050).
#[derive(Debug, Clone, Copy)]
pub struct GcConfig {
    /// How long a TERMINAL run's workspace stays reachable, in ms.
    pub workspace_ttl_ms: i64,
    /// Objects younger than this are never swept even when unmarked — the
    /// window protecting an in-flight ingest whose root is not yet recorded.
    pub grace_ms: i64,
}

const GC_LEASE: &str = "cas-gc";
const GC_LEASE_TTL_MS: i64 = 5 * 60 * 1000;

/// One mark-sweep pass over the workspace CAS (ADR-0050). Returns the number
/// of objects swept. Leader-gated on its own lease.
///
/// - **Mark**: walk every root the Db reports reachable (all non-terminal
///   runs + terminal runs within TTL), collecting `trees/<h>` + `blobs/<h>`.
///   A transient walk error aborts the pass — robust over precise: a missed
///   mark must never become a deleted live object. A MISSING root (dangling
///   reference to a wiped tree) is skipped instead, so one lost object can't
///   wedge GC forever.
/// - **Sweep**: delete unmarked objects older than the grace window.
pub async fn sweep_cas(
    db: &Arc<dyn Db>,
    cas: &Arc<dyn scarab_storage::Cas>,
    store: &Arc<dyn ObjectStore>,
    clock: &Arc<dyn Clock>,
    owner: &str,
    cfg: GcConfig,
) -> Result<u32, String> {
    let lease = db
        .lease(GC_LEASE, owner, GC_LEASE_TTL_MS)
        .await
        .map_err(|e| e.to_string())?;
    if lease.owner != owner {
        return Ok(0);
    }
    let now = clock.now().await;

    // --- Mark. ---------------------------------------------------------
    let roots = db
        .gc_workspace_roots(Timestamp(now.0 - cfg.workspace_ttl_ms))
        .await
        .map_err(|e| e.to_string())?;
    let mut marked: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut frontier: Vec<String> = roots;
    while let Some(hash) = frontier.pop() {
        if !marked.insert(format!("trees/{hash}")) {
            continue; // shared subtree already walked (the dedup win)
        }
        let entries = match cas
            .tree_entries(&scarab_storage::TreeHash(hash.clone()))
            .await
        {
            Ok(entries) => entries,
            // A MISSING tree is a dangling reference (e.g. a run recorded before
            // the object store was switched, whose blobs were wiped): the
            // subtree doesn't exist, so there is nothing under it to mark and
            // skipping it cannot endanger the sweep — anything it would have
            // referenced is itself absent (and thus already garbage). Log and
            // continue, so one lost object can't wedge GC forever. ANY OTHER
            // error may be transient over a tree that DOES exist, where an
            // unmarked-but-live blob could then be swept — stay conservative
            // and abort the pass (a missed mark must never delete a live object).
            Err(scarab_storage::StorageError::NotFound) => {
                tracing::warn!(tree = %hash, "cas gc: root tree missing (dangling reference) — skipping");
                continue;
            }
            Err(e) => return Err(format!("mark walk of tree {hash}: {e} — aborting pass")),
        };
        for entry in entries {
            match entry.target {
                scarab_storage::TreeTarget::Blob(b) => {
                    marked.insert(format!("blobs/{}", b.0));
                }
                scarab_storage::TreeTarget::Tree(t) => frontier.push(t.0),
            }
        }
    }

    // --- Sweep. --------------------------------------------------------
    let mut swept = 0u32;
    for prefix in ["trees/", "blobs/"] {
        let objects = store
            .list_objects(prefix)
            .await
            .map_err(|e| e.to_string())?;
        for obj in objects {
            if marked.contains(&obj.key) {
                continue;
            }
            if now.0 - obj.modified_ms < cfg.grace_ms {
                continue; // too young — possibly an in-flight ingest
            }
            if let Err(e) = store.delete(&obj.key).await {
                tracing::warn!(key = %obj.key, error = %e, "cas gc: delete failed (next pass retries)");
                continue;
            }
            swept += 1;
        }
    }
    if swept > 0 {
        tracing::info!(swept, marked = marked.len(), "cas gc pass complete");
    }
    Ok(swept)
}

/// Spawn the background sweeper: one retention pass + one CAS GC pass every
/// `interval`, forever.
pub fn spawn_sweeper(
    db: Arc<dyn Db>,
    cas: Arc<dyn scarab_storage::Cas>,
    store: Arc<dyn ObjectStore>,
    clock: Arc<dyn Clock>,
    owner: String,
    cfg: RetentionConfig,
    gc: GcConfig,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match sweep_retention(&db, &store, &clock, &owner, cfg).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(runs = n, "retention sweep pass complete"),
                Err(e) => tracing::warn!(error = %e, "retention sweep pass failed"),
            }
            if let Err(e) = sweep_cas(&db, &cas, &store, &clock, &owner, gc).await {
                tracing::warn!(error = %e, "cas gc pass failed");
            }
            tokio::time::sleep(interval).await;
        }
    })
}
