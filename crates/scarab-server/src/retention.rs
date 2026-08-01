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

/// The [`WorkspaceSnapshots`](scarab_engine::WorkspaceSnapshots) adapter over the
/// workspace CAS (ADR-0061 s5): the one bit of truth the pure engine needs about
/// the cold tier — *can this Workspace Snapshot still be materialised?*
///
/// The check is the presence of the **root tree object**, not a full walk. That is
/// sound under mark-sweep (ADR-0050) rather than merely cheap: the sweep deletes
/// unmarked objects, and a root is unmarked exactly when nothing reachable
/// references it — in which case its exclusive subtree is unmarked too and goes
/// with it, while anything that survived did so because something else marked it.
/// So "root present" and "tree materialisable" coincide, except in a genuinely
/// torn CAS — which the executor's own input-missing fail-fast still catches. A
/// transitive walk would cost one round-trip per tree on a path a human is
/// waiting on, to defend against a case that is already covered.
///
/// **This must stay a COLD-tier question.** Behind a tiered `Cas`
/// (`scarab_storage::tiered`) `tree_entries` reads warm, falls back to cold, and
/// reports `NotFound` only when *both* miss — which is exactly the semantics this
/// needs. A warm miss alone must never widen a rerun: the warm tier is bounded by
/// space and evicts LRU, so a miss there is slower and never wrong, and widening
/// on one would turn ordinary cache pressure into surprise full-pipeline re-runs.
/// If anything ever gives this a warm-only view of the store, that is a bug here,
/// not a behaviour change.
pub struct CasSnapshots(pub Arc<dyn scarab_storage::Cas>);

#[async_trait::async_trait]
impl scarab_engine::WorkspaceSnapshots for CasSnapshots {
    async fn snapshot_present(&self, root: &str) -> bool {
        match self
            .0
            .tree_entries(&scarab_storage::TreeHash(root.to_string()))
            .await
        {
            Ok(_) => true,
            Err(scarab_storage::StorageError::NotFound) => false,
            // NOT proof of absence. Only a definitive not-found may widen a
            // rerun; treating a store blip as an expiry would re-run a whole
            // pipeline from `clone` because of one bad TCP connection. Assume
            // present, say so loudly, and let the executor's input-missing
            // fail-fast be the backstop if it really is gone.
            Err(e) => {
                tracing::warn!(
                    root = %root,
                    error = %e,
                    "workspace snapshot presence check failed — assuming PRESENT (a transient \
                     error is not proof of expiry)"
                );
                true
            }
        }
    }
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

/// One reachable object the mark walk could read (through the tiered handle —
/// which may have been served by the WARM tier) that the cold listing does not
/// contain: the torn-cold detection of ticket d4d3b95. Every one of these is a
/// live object one warm-volume failure away from being unrecoverable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColdResidue {
    /// The missing object's cold key (`trees/<hash>` or `blobs/<hash>`).
    pub key: String,
    /// The FIRST root whose walk reached the object — where an operator
    /// starts looking. First-writer-wins is exactly the order the walk's
    /// shared-subtree short-circuit already imposes, so recording it is free.
    pub root: String,
    /// When that root was recorded — the suppression clock.
    pub root_recorded_at: Timestamp,
}

/// What one CAS GC pass did — and what it found missing.
#[derive(Debug, Default)]
pub struct CasSweepReport {
    /// Unmarked cold objects deleted.
    pub swept: u32,
    /// Marked objects ABSENT from the cold listing, alarmed at error level:
    /// cold is silently missing reachable data and only the warm tier is
    /// keeping reads (and the mark walk itself) green.
    pub residue: Vec<ColdResidue>,
    /// Residue whose first-marking root is younger than the grace window: an
    /// ADR-0064 cold flush may still be in flight, so it is counted and
    /// logged at debug level, never alarmed.
    pub suppressed_residue: Vec<ColdResidue>,
}

/// Bound on per-object residue error lines in one pass — the alarm must not
/// become a thousand-line spam when a whole volume tears.
const RESIDUE_LOG_CAP: usize = 50;

/// One mark-sweep pass over the workspace CAS (ADR-0050). Returns what it did
/// and what it found missing ([`CasSweepReport`]). Leader-gated on its own
/// lease; a non-leader replica returns an empty report.
///
/// - **Mark**: walk every root the Db reports reachable (all non-terminal
///   runs + terminal runs within TTL), collecting `trees/<h>` + `blobs/<h>`.
///   A transient walk error aborts the pass — robust over precise: a missed
///   mark must never become a deleted live object. A MISSING root (dangling
///   reference to a wiped tree) is skipped instead, so one lost object can't
///   wedge GC forever, and the Db then FORGETS that root so the skip is
///   reported once rather than on every pass forever.
/// - **Sweep**: delete unmarked objects older than the grace window.
/// - **Torn-cold detection** (ticket d4d3b95): the mark walk reads through
///   the TIERED handle, so an object the warm tier still holds marks clean
///   even when cold silently lost it. The residue diff `marked − cold
///   listing` (the sweep already lists all of cold) is exactly that hole;
///   each entry is alarmed with its address and first-marking root.
///   Detection only — the objects stay marked (so the sweep cannot delete
///   them) and re-upload-from-warm is a filed follow-up, not this slice.
pub async fn sweep_cas(
    db: &Arc<dyn Db>,
    cas: &Arc<dyn scarab_storage::Cas>,
    store: &Arc<dyn ObjectStore>,
    clock: &Arc<dyn Clock>,
    owner: &str,
    cfg: GcConfig,
) -> Result<CasSweepReport, String> {
    let lease = db
        .lease(GC_LEASE, owner, GC_LEASE_TTL_MS)
        .await
        .map_err(|e| e.to_string())?;
    if lease.owner != owner {
        return Ok(CasSweepReport::default());
    }
    let now = clock.now().await;

    // --- Mark. ---------------------------------------------------------
    let roots = db
        .gc_workspace_roots(Timestamp(now.0 - cfg.workspace_ttl_ms))
        .await
        .map_err(|e| e.to_string())?;
    // Which hashes came straight from the Db, as opposed to being discovered
    // under a parent tree: only a ROOT is a reference the Db can forget.
    let root_set: std::collections::HashSet<String> =
        roots.iter().map(|(h, _)| h.clone()).collect();
    // Marked key → index (into `roots`) of the FIRST root whose walk reached
    // it. Walking per-root and recording provenance only on first insertion
    // keeps the shared-subtree short-circuit — and thus the walk cost —
    // exactly what it was; the provenance is the order the dedup already chose.
    let mut marked: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // Keys the walk itself proved absent from BOTH tiers (dangling roots, torn
    // inner trees). Each is reported on its own path below and must not ALSO
    // surface as cold residue: residue means "warm still has it, cold does
    // not", and these have neither.
    let mut walk_missing: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut dangling_roots: Vec<String> = Vec::new();
    for (root_idx, (root, _)) in roots.iter().enumerate() {
        let mut frontier: Vec<String> = vec![root.clone()];
        while let Some(hash) = frontier.pop() {
            let key = format!("trees/{hash}");
            if marked.contains_key(&key) {
                continue; // shared subtree already walked (the dedup win)
            }
            marked.insert(key.clone(), root_idx);
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
                //
                // NOTE this NotFound came through the TIERED read: it means BOTH
                // tiers miss the tree. A root that cold lost but warm still holds
                // reads fine here, is never pushed to `dangling_roots`, and so is
                // never forgotten — it surfaces as residue below instead.
                Err(scarab_storage::StorageError::NotFound) if root_set.contains(&hash) => {
                    tracing::warn!(
                        tree = %hash,
                        "cas gc: root tree missing (dangling reference) — forgetting"
                    );
                    walk_missing.insert(key);
                    dangling_roots.push(hash);
                    continue;
                }
                Err(scarab_storage::StorageError::NotFound) => {
                    // NOT a root: a parent tree that DOES exist referenced this
                    // subtree, so the CAS is partially torn rather than merely
                    // stale. There is nothing under it to mark (so the sweep stays
                    // safe) and no Db reference to forget — but unlike a stale root
                    // this is real corruption, so say so at a louder level.
                    tracing::error!(
                        tree = %hash,
                        "cas gc: inner tree missing under a live parent (torn CAS) — skipping"
                    );
                    walk_missing.insert(key);
                    continue;
                }
                Err(e) => return Err(format!("mark walk of tree {hash}: {e} — aborting pass")),
            };
            for entry in entries {
                match entry.target {
                    scarab_storage::TreeTarget::Blob(b) => {
                        marked.entry(format!("blobs/{}", b.0)).or_insert(root_idx);
                    }
                    scarab_storage::TreeTarget::Tree(t) => frontier.push(t.0),
                }
            }
        }
    }

    // --- Self-heal. ----------------------------------------------------
    // Drop the references the walk PROVED dead, so each lost root is reported
    // once instead of re-walked and re-warned every pass forever. Deliberately
    // after the walk: an aborted walk must never mutate run history, and only
    // a definitive NotFound over a root gets here.
    for root in &dangling_roots {
        match db.forget_workspace_root(root).await {
            Ok(cleared) => {
                tracing::info!(tree = %root, cleared, "cas gc: forgot dangling workspace root")
            }
            // Non-fatal: the sweep below is still correct (the root's subtree is
            // absent either way) and the next pass retries the forget.
            Err(e) => {
                tracing::warn!(tree = %root, error = %e, "cas gc: could not forget dangling root")
            }
        }
    }

    // --- Sweep. --------------------------------------------------------
    // The listing is COLD's, and it is complete (`S3Storage::list_objects`
    // collects the full paginated stream) — so it doubles as the durable-set
    // census the residue diff below needs.
    let mut swept = 0u32;
    let mut in_cold: std::collections::HashSet<String> = std::collections::HashSet::new();
    for prefix in ["trees/", "blobs/"] {
        let objects = store
            .list_objects(prefix)
            .await
            .map_err(|e| e.to_string())?;
        for obj in objects {
            if marked.contains_key(&obj.key) {
                in_cold.insert(obj.key);
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

    // --- Torn-cold detection (ticket d4d3b95). --------------------------
    // Residue is `marked − cold listing`, and ONLY that direction: cold-extra
    // objects are the ordinary sweep candidates above, never residue. Every
    // entry here read fine through the tiered handle (i.e. warm still holds
    // it) yet is absent from the durable tier — one warm-volume failure from
    // unrecoverable. The objects stay marked, so the sweep above cannot have
    // deleted them, and their roots were never proven dead, so the self-heal
    // above cannot have forgotten them. Detection only in this slice.
    let mut residue: Vec<ColdResidue> = Vec::new();
    let mut suppressed_residue: Vec<ColdResidue> = Vec::new();
    for (key, root_idx) in &marked {
        if in_cold.contains(key) || walk_missing.contains(key) {
            continue;
        }
        let (root, recorded_at) = &roots[*root_idx];
        let item = ColdResidue {
            key: key.clone(),
            root: root.clone(),
            root_recorded_at: *recorded_at,
        };
        // A root recorded moments ago may have its cold flush still in flight
        // (ADR-0064): count it, but do not cry wolf. Deliberately the SAME
        // window the sweep already trusts for in-flight ingests — no new knob.
        if now.0 - recorded_at.0 < cfg.grace_ms {
            suppressed_residue.push(item);
        } else {
            residue.push(item);
        }
    }
    residue.sort_by(|a, b| a.key.cmp(&b.key));
    suppressed_residue.sort_by(|a, b| a.key.cmp(&b.key));
    for r in residue.iter().take(RESIDUE_LOG_CAP) {
        tracing::error!(
            object = %r.key,
            root = %r.root,
            "cas gc: reachable object MISSING from cold storage (torn cold tier) — only \
             the warm tier still holds it, and it becomes unrecoverable if that volume \
             dies; re-upload from warm is a filed follow-up, not automated here"
        );
    }
    if residue.len() > RESIDUE_LOG_CAP {
        tracing::error!(
            total = residue.len(),
            logged = RESIDUE_LOG_CAP,
            "cas gc: more torn-cold residue than the per-pass log cap"
        );
    }
    for r in &suppressed_residue {
        tracing::debug!(
            object = %r.key,
            root = %r.root,
            root_recorded_at = r.root_recorded_at.0,
            "cas gc: cold residue under a root younger than the grace window — possibly \
             an ADR-0064 flush still in flight; suppressed"
        );
    }
    // Gauge-like, SET per pass rather than accumulated, so a repaired cold
    // tier is visible as the value returning to zero.
    crate::metrics::set_cas_gc_cold_residue(
        residue.len() as u64,
        suppressed_residue.len() as u64,
    );

    if swept > 0 {
        tracing::info!(swept, marked = marked.len(), "cas gc pass complete");
    }
    Ok(CasSweepReport {
        swept,
        residue,
        suppressed_residue,
    })
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
