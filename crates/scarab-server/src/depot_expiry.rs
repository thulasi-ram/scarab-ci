//! Committed-fence retention expiry (git-bug 6499fb1; ADR-0065, ADR-0067).
//!
//! The CONTROL PLANE selects and executes: victim selection needs `runs.status`,
//! `runs.updated_at`, `runs.snapshots_pinned_at` — all control-plane-owned
//! policy — while the deletion itself is pure SQL over the `depot_*` tables in
//! the same Postgres. This pass deletes **POINTERS ONLY** (the four row
//! families of a fence: `depot_pack_members`, `depot_packs`,
//! `depot_drain_records`, and the fence's *outbound* `depot_fence_borrows`);
//! the bytes go rowless and the Depot's shipped orphan reclaimer
//! (`reclaim_orphan_packs_once`, git-bug ad79c90) collects them behind its
//! two-observation grace, at least one cadence later. This module is the ONE
//! licensed deleter of committed pack rows — see the STOP LINE above
//! `pack_reclaim_pass` in `workspaced.rs`.
//!
//! **The deletion contract** (defined by git-bug ec294b7): fence F is
//! deletable when its run is terminal and past the pack-retention TTL and not
//! pinned, AND no borrow edge on F has a borrower whose drain record still
//! lives (borrower-record lifetime = borrower-fence lifetime; the fence
//! residue sweep exempts post-epoch success records for exactly this reason).
//! Deletion is one transaction PER VICTIM FENCE (audit A7 — never batch
//! fences), in this order: `FOR UPDATE` F's `depot_packs` rows FIRST (the
//! drain path's in-transaction re-check takes `FOR SHARE OF p` on the same
//! rows — that lock order is what serializes the two passes in both
//! directions, pinned by `the_share_lock_serializes_record_and_expiry_in_both_
//! orders`), then re-read the FULL candidate predicate, then the borrower
//! check, then the deletes. A deadlock (SQLSTATE `40P01`) anywhere aborts the
//! WHOLE pass — the next cadence retries.
//!
//! **The pre-epoch reachability floor** (the ec294b7 backfill floor, as
//! amended 2026-08-26): drains that predate `depot_borrow_tracking_epoch`
//! recorded no borrow edges, so a pre-epoch borrower may silently depend on
//! anything. The saving lemma: a pre-epoch borrower's owners are necessarily
//! pre-epoch too (an owner's packs committed before the borrower's POST,
//! which predates the epoch), so unrecorded borrows only ever target packs
//! with `created_at < epoch`. Therefore the floor SCOPES to pre-epoch victims
//! only — a fence whose packs predate the epoch, and the committed-recordless
//! arm below — while post-epoch fences expire on borrow edges alone, from day
//! one. The floor itself is control-plane REACHABILITY, not record-existence:
//! pre-epoch committed content is deletable only when NO pre-epoch run
//! (`runs.created_at < epoch`) is still reachable — non-terminal, or within
//! the workspace TTL, or pinned. Records stay on their 24h-scale residue
//! sweep; the floor is not keyed on them, so it drains on the workspace TTL
//! rather than deadlocking on an exemption.
//!
//! **The committed-recordless arm** (audit A3, folded into the same floor):
//! committed packs with NO drain record and `created_at < epoch` are pre-epoch
//! debris whose record was residue-swept before fence expiry existed. They are
//! deletable ONLY once that same reachability floor has drained — one arm, one
//! predicate, never a standalone rule — and still only through the per-victim
//! transaction's borrower check.
//!
//! **Units** (audit f4): `runs.updated_at`/`runs.created_at` are epoch
//! MILLIS; `depot_packs.created_at`, `depot_drain_records.posted_at` and the
//! epoch are epoch SECONDS. Every cutoff is derived from Postgres `now()` —
//! one clock authority — and converted through [`cutoff_ms`], which has its
//! own named test.
//!
//! A rerun can flip a terminal run back to non-terminal, so candidacy read
//! outside the victim transaction is advisory: the transaction re-reads the
//! full predicate before deleting (audit A2). No `runs`-row lock is taken —
//! the residual millisecond race is retention *semantics*, not a race: a
//! rerun of a retention-expired run must re-derive its ancestors or fail
//! loudly on reads (the ADR-0061 widened-rerun contract).

use sqlx::PgPool;

use crate::workspaced::PACK_RECLAIM_ADVISORY_LOCK;

/// Victim fences processed per arm per pass — bounds one pass's work; the
/// loop catches up. Same figure as the retention sweeper's `SWEEP_BATCH`.
const EXPIRY_BATCH: u32 = 100;

/// The DB spellings of the terminal run states, as one SQL `IN` list. The
/// same literal set every retention query in `scarab-db-postgres` uses
/// (`prunable_log_runs` et al.); `terminal_set_matches_the_domain` below
/// verifies it against `RunStatus::is_terminal` so a new run state cannot
/// silently widen or narrow expiry.
const TERMINAL_RUN_STATUSES_SQL: &str = "('succeeded', 'failed', 'cancelled', 'dead_lettered')";

/// The Rust-side twin of [`TERMINAL_RUN_STATUSES_SQL`], for the in-transaction
/// re-read (which fetches the row and evaluates the predicate in code so the
/// S2 per-profile cutoffs slot in without another query shape).
fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "succeeded" | "failed" | "cancelled" | "dead_lettered"
    )
}

/// The flat class TTLs, in seconds — the env knobs, and the fallback for any
/// class a [`RetentionProfile`](scarab_pipeline::RetentionProfile) leaves
/// unset.
#[derive(Debug, Clone, Copy)]
pub struct ExpiryTtls {
    /// How long a terminal run's PACKS are kept (`SCARAB_RETENTION_PACK_DAYS`).
    /// Boot-validated ≥ the workspace TTL: packs back workspace snapshots, so
    /// a shorter pack TTL would unback durable materialization while roots
    /// are still reachable.
    pub pack_ttl_secs: i64,
    /// The workspace-CAS reachability TTL (`SCARAB_RETENTION_WORKSPACE_DAYS`)
    /// — the "still reachable" window of the pre-epoch floor.
    pub workspace_ttl_secs: i64,
}

/// The operator retention config file (`SCARAB_RETENTION_CONFIG_FILE`,
/// ADR-0065 s2): the named [`RetentionProfile`](scarab_pipeline::
/// RetentionProfile) registry, YAML/JSON, gitops-managed — the
/// `SCARAB_PLACEMENT_CONFIG_FILE` shape exactly. A bad path/parse/validation
/// is a boot failure (ADR-0048).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RetentionConfigFile {
    #[serde(default)]
    pub profiles: Vec<scarab_pipeline::RetentionProfile>,
}

/// The per-run TTL source (ADR-0065 s2): the operator's named profiles over
/// the flat env fallbacks. A run names a profile through its IR
/// (`runs.ir->>'retention_profile'`), and the name is resolved against the
/// CURRENT registry at sweep time — an operator retune applies retroactively,
/// which is the point. Resolution: named profile; unknown name → warn + the
/// `default`-flagged profile; no name → the `default` profile; no default →
/// the flat env TTLs. A profile field left unset falls back per-class to the
/// flat value.
#[derive(Debug, Clone)]
pub struct RetentionRegistry {
    profiles: Vec<scarab_pipeline::RetentionProfile>,
    flat: ExpiryTtls,
}

/// Days → seconds, for the profile fields (the env knobs arrive pre-scaled).
fn days_secs(days: u32) -> i64 {
    i64::from(days) * 24 * 60 * 60
}

impl RetentionRegistry {
    /// No profiles configured: every run ages under the flat env TTLs.
    pub fn flat(flat: ExpiryTtls) -> Self {
        Self {
            profiles: Vec::new(),
            flat,
        }
    }

    /// A validated registry: unique non-empty names, at most one `default`
    /// (the shared [`scarab_pipeline::validate_profile_registry`] machinery),
    /// and per profile the same floor the env knobs obey — resolved pack TTL
    /// ≥ resolved workspace TTL, because packs back Workspace Snapshots.
    pub fn new(
        profiles: Vec<scarab_pipeline::RetentionProfile>,
        flat: ExpiryTtls,
    ) -> Result<Self, String> {
        scarab_pipeline::validate_profile_registry(&profiles, "retention")?;
        let registry = Self { profiles, flat };
        for p in &registry.profiles {
            let ttls = registry.ttls_of_profile(Some(p));
            if ttls.pack_ttl_secs < ttls.workspace_ttl_secs {
                return Err(format!(
                    "retention profile `{}` resolves pack TTL below the workspace TTL \
                     ({}s < {}s) — packs back Workspace Snapshots, so the pack TTL \
                     must be >= it (ADR-0065/0067)",
                    p.name, ttls.pack_ttl_secs, ttls.workspace_ttl_secs
                ));
            }
        }
        Ok(registry)
    }

    /// The TTLs one run ages under, from its (possibly absent) profile name.
    fn ttls_of(&self, name: Option<&str>) -> ExpiryTtls {
        let profile = match name {
            Some(n) => match scarab_pipeline::profile_named(&self.profiles, n) {
                Some(p) => Some(p),
                None => {
                    tracing::warn!(
                        profile = %n,
                        "depot expiry: run names a retention profile absent from the \
                         current registry — falling back to the default profile / flat \
                         TTLs (the registry is the operator's; renames apply \
                         retroactively)"
                    );
                    scarab_pipeline::default_profile(&self.profiles)
                }
            },
            None => scarab_pipeline::default_profile(&self.profiles),
        };
        self.ttls_of_profile(profile)
    }

    fn ttls_of_profile(&self, profile: Option<&scarab_pipeline::RetentionProfile>) -> ExpiryTtls {
        match profile {
            Some(p) => ExpiryTtls {
                pack_ttl_secs: p
                    .pack_ttl_days
                    .map(days_secs)
                    .unwrap_or(self.flat.pack_ttl_secs),
                workspace_ttl_secs: p
                    .workspace_ttl_days
                    .map(days_secs)
                    .unwrap_or(self.flat.workspace_ttl_secs),
            },
            None => self.flat,
        }
    }

    /// The pack TTL a run with this profile name ages under, in seconds.
    pub fn pack_ttl_secs(&self, name: Option<&str>) -> i64 {
        self.ttls_of(name).pack_ttl_secs
    }

    /// Every profile's (name, resolved pack TTL) pair — the nomination
    /// query's CASE arms, rebuilt from the CURRENT registry at every pass
    /// start (which is what keeps an ADR-0065 s2 operator retune
    /// retroactive). The registry is small; the CASE is bounded by it.
    fn profile_pack_ttls(&self) -> Vec<(&str, i64)> {
        self.profiles
            .iter()
            .map(|p| (p.name.as_str(), self.ttls_of_profile(Some(p)).pack_ttl_secs))
            .collect()
    }

    /// The LONGEST workspace TTL any resolution can produce — the pre-epoch
    /// reachability floor must hold while a pre-epoch run is reachable under
    /// ANY profile, so the floor uses the conservative maximum.
    fn max_workspace_ttl_secs(&self) -> i64 {
        self.profiles
            .iter()
            .map(|p| self.ttls_of_profile(Some(p)).workspace_ttl_secs)
            .fold(self.flat.workspace_ttl_secs, i64::max)
    }
}

/// MILLIS cutoff from a SECONDS clock and TTL — the one place the units meet
/// (`runs.updated_at` is millis; `now()` and the TTLs are seconds). Named and
/// tested on its own because a seconds-vs-millis slip here silently makes
/// every run a victim (cutoff ~1970) or none (cutoff ~year 52000).
fn cutoff_ms(now_secs: i64, ttl_secs: i64) -> i64 {
    now_secs.saturating_sub(ttl_secs).saturating_mul(1000)
}

/// Test-only injection point: SQL executed on the victim transaction's own
/// connection right after the `FOR UPDATE`, when the victim's fence key
/// matches — how the tests construct the interleavings this pass must survive
/// (a rerun flipping the run mid-pass; a raised `40P01`) instead of trying to
/// schedule them.
#[cfg(test)]
pub(crate) static TEST_INJECT_IN_VICTIM_TXN: std::sync::Mutex<Option<(String, String)>> =
    std::sync::Mutex::new(None);

/// Spawn the expiry loop: one [`expire_committed_fences_once`] pass every
/// `interval`, forever, on its own small pool (a composition-root concern —
/// the pass holds one connection for the advisory lock plus one per victim
/// transaction).
pub fn spawn_expiry(
    db: PgPool,
    registry: RetentionRegistry,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match expire_committed_fences_once(&db, &registry, EXPIRY_BATCH).await {
                Ok(ExpiryPass::LockBusy) | Ok(ExpiryPass::Ran { expired: 0 }) => {}
                Ok(ExpiryPass::Ran { expired }) => tracing::info!(
                    fences = expired,
                    "depot expiry: expired committed fences — pointers deleted, bytes \
                     left for the rowless reclaimer (git-bug 6499fb1)"
                ),
                Err(e) => {
                    crate::metrics::record_depot_expiry_pass_skipped();
                    tracing::error!(
                        error = %e,
                        "depot expiry: pass failed — nothing further was deleted \
                         (fail-closed); the next pass retries"
                    );
                }
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// One expiry pass. Serialized across replicas AND against the Depot's own
/// reclaimer by [`PACK_RECLAIM_ADVISORY_LOCK`], taken on one held connection
/// for the whole pass (economy against the reclaimer — every delete is
/// row-guarded — and hygiene against a second expiry replica). Not acquired =
/// another pass is running: debug, not a skip. Returns the number of fences
/// expired.
pub async fn expire_committed_fences_once(
    db: &PgPool,
    registry: &RetentionRegistry,
    batch: u32,
) -> Result<ExpiryPass, sqlx::Error> {
    // The advisory lock lives on ONE dedicated pooled connection — session
    // scoped, so the connection is held for the pass; if the explicit unlock
    // fails the connection is closed rather than returned to the pool still
    // holding the lock (the `pack_reclaim_pass` discipline).
    let mut conn = db.acquire().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(PACK_RECLAIM_ADVISORY_LOCK)
        .fetch_one(&mut *conn)
        .await?;
    if !locked {
        tracing::debug!(
            "depot expiry: another session holds the pass lock (a replica's pass, or \
             the Depot reclaimer) — not a skip, the work is happening elsewhere"
        );
        return Ok(ExpiryPass::LockBusy);
    }

    let result = expire_pass(db, registry, batch).await;

    match sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(PACK_RECLAIM_ADVISORY_LOCK)
        .execute(&mut *conn)
        .await
    {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                error = %e,
                "depot expiry: advisory unlock failed — closing the connection so the \
                 pool never re-serves a session that still holds the pass lock"
            );
            let _ = sqlx::Connection::close(conn.detach()).await;
        }
    }
    result.map(|expired| ExpiryPass::Ran { expired })
}

/// What one call to [`expire_committed_fences_once`] did. `LockBusy` is a
/// DISTINCT outcome, never folded into "ran and found nothing": the two mean
/// different things to an operator (work happening elsewhere vs nothing
/// eligible) and to a test (retry vs a real verdict).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryPass {
    /// The pass held the lock and ran. `expired` counts the fences whose
    /// victim transactions committed — including a pass a 40P01 aborted
    /// early, which counts what committed before the abort.
    Ran { expired: u32 },
    /// Another session held [`PACK_RECLAIM_ADVISORY_LOCK`]; nothing was
    /// examined and nothing was deleted.
    LockBusy,
}

/// The pass body, under the advisory lock: the floor, then the recorded arm,
/// then (floor permitting) the recordless arm.
async fn expire_pass(
    db: &PgPool,
    registry: &RetentionRegistry,
    batch: u32,
) -> Result<u32, sqlx::Error> {
    // One clock authority: Postgres `now()`, read beside the epoch (stamped
    // once, from that same clock, when migration 0048 ran).
    let (now_secs, epoch): (i64, i64) = sqlx::query_as(
        "SELECT EXTRACT(EPOCH FROM now())::bigint, epoch FROM depot_borrow_tracking_epoch",
    )
    .fetch_one(db)
    .await?;
    let ws_cutoff = cutoff_ms(now_secs, registry.max_workspace_ttl_secs());
    let epoch_ms = epoch.saturating_mul(1000);

    // The pre-epoch reachability floor (module docs): held while ANY
    // pre-epoch run is still reachable — non-terminal, within the workspace
    // TTL, or pinned. Pre-epoch runs are a fixed, shrinking set, so this
    // drains with time and then costs one indexed EXISTS forever.
    let floor_held: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS ( \
             SELECT 1 FROM runs ru \
             WHERE ru.created_at < $1 \
               AND (ru.status NOT IN {TERMINAL_RUN_STATUSES_SQL} \
                    OR ru.updated_at >= $2 \
                    OR ru.snapshots_pinned_at IS NOT NULL))"
    ))
    .bind(epoch_ms)
    .bind(ws_cutoff)
    .fetch_one(db)
    .await?;
    crate::metrics::set_depot_expiry_floor_held(floor_held);

    let mut expired = 0u32;
    // Nominated candidates the pass declined without expiring (the victim
    // transaction's re-read said no: a live borrower, a rerun flip, a
    // vanished record). Recorded per pass so a starved window — skips
    // climbing while `expired` stays flat — is VISIBLE, never silent
    // (git-bug a543fef).
    let mut skipped = 0u32;

    // --- The recorded arm: success records whose run is terminal, past ITS
    // OWN profile's pack TTL and unpinned. The per-run cutoff is resolved
    // INTO the query (git-bug a543fef): one CASE arm per registry profile,
    // built at pass start. The pre-s2 shape — nominate under the loosest
    // cutoff, `continue` in code — starved the window: >= `batch` old
    // long-TTL runs sat first in posted_at order, every pass re-fetched
    // exactly them, and genuinely-expired victims behind them were never
    // nominated until the blockers aged out. The pre-epoch floor gate is a
    // WHERE clause ($2/$3) for the same reason: a floor-held backlog must
    // not occupy the window either. A post-epoch fence (no pack older than
    // the epoch) expires on its recorded edges alone even while the floor
    // is up.
    let profile_cutoffs: Vec<(&str, i64)> = registry
        .profile_pack_ttls()
        .into_iter()
        .map(|(name, ttl)| (name, cutoff_ms(now_secs, ttl)))
        .collect();
    // No name and an unknown name both resolve to the default profile /
    // flat TTLs — CASE's ELSE (a NULL scrutinee never matches a WHEN); the
    // unknown-name warn still fires in the victim transaction's re-read.
    let default_cutoff = cutoff_ms(now_secs, registry.pack_ttl_secs(None));
    let cutoff_expr = if profile_cutoffs.is_empty() {
        // `CASE x ELSE y END` with zero WHEN arms is not SQL.
        "$4".to_string()
    } else {
        let mut expr = String::from("CASE ru.ir->>'retention_profile' ");
        let mut n = 5;
        for _ in &profile_cutoffs {
            expr.push_str(&format!("WHEN ${n} THEN ${} ", n + 1));
            n += 2;
        }
        expr.push_str("ELSE $4 END");
        expr
    };
    let sql = format!(
        "SELECT r.fence_key \
         FROM depot_drain_records r \
         JOIN runs ru ON ru.id = r.run \
         WHERE r.record->>'error' IS NULL \
           AND ru.status IN {TERMINAL_RUN_STATUSES_SQL} \
           AND ru.snapshots_pinned_at IS NULL \
           AND ru.updated_at < {cutoff_expr} \
           AND (NOT $2 OR NOT EXISTS \
                (SELECT 1 FROM depot_packs p \
                 WHERE p.fence_key = r.fence_key AND p.created_at < $3)) \
         ORDER BY r.posted_at \
         LIMIT $1"
    );
    let mut query = sqlx::query_scalar::<_, String>(&sql)
        .bind(i64::from(batch))
        .bind(floor_held)
        .bind(epoch)
        .bind(default_cutoff);
    for (name, cutoff) in &profile_cutoffs {
        query = query.bind(*name).bind(*cutoff);
    }
    let candidates: Vec<String> = query.fetch_all(db).await?;
    for fence in candidates {
        match expire_one_fence(db, &fence, Victim::Recorded { now_secs }, registry).await {
            Ok(true) => expired += 1,
            Ok(false) => skipped += 1,
            Err(e) if is_deadlock(&e) => {
                crate::metrics::record_depot_expiry_pass_skipped();
                crate::metrics::record_depot_expiry_candidates_skipped(skipped);
                tracing::warn!(
                    fence_key = %fence,
                    "depot expiry: deadlock (40P01) in a victim transaction — pass \
                     ABORTED, nothing further deleted; the next cadence retries"
                );
                return Ok(expired);
            }
            Err(e) => return Err(e),
        }
    }

    // --- The committed-recordless arm (audit A3): pre-epoch debris whose
    // record was residue-swept before this pass existed. Gated on the SAME
    // floor — never a standalone rule — and `HAVING max(created_at) < epoch`
    // so a fence with any post-epoch pack (whose record should exist) never
    // qualifies.
    if !floor_held {
        let recordless: Vec<String> = sqlx::query_scalar(
            "SELECT p.fence_key FROM depot_packs p \
             WHERE p.committed \
               AND NOT EXISTS (SELECT 1 FROM depot_drain_records r \
                               WHERE r.fence_key = p.fence_key) \
             GROUP BY p.fence_key \
             HAVING max(p.created_at) < $1 \
             ORDER BY max(p.created_at) \
             LIMIT $2",
        )
        .bind(epoch)
        .bind(i64::from(batch))
        .fetch_all(db)
        .await?;
        for fence in recordless {
            match expire_one_fence(db, &fence, Victim::Recordless { epoch }, registry).await {
                Ok(true) => expired += 1,
                Ok(false) => skipped += 1,
                Err(e) if is_deadlock(&e) => {
                    crate::metrics::record_depot_expiry_pass_skipped();
                    crate::metrics::record_depot_expiry_candidates_skipped(skipped);
                    tracing::warn!(
                        fence_key = %fence,
                        "depot expiry: deadlock (40P01) in a victim transaction — pass \
                         ABORTED, nothing further deleted; the next cadence retries"
                    );
                    return Ok(expired);
                }
                Err(e) => return Err(e),
            }
        }
    }

    crate::metrics::record_depot_expiry_candidates_skipped(skipped);
    Ok(expired)
}

/// Which arm nominated a victim — and therefore which predicate the victim
/// transaction re-reads before deleting.
enum Victim {
    /// A live success record whose run answered the candidate query. Carries
    /// the pass clock; the cutoff is re-resolved per run INSIDE the victim
    /// transaction (the profile name is re-read with the row).
    Recorded { now_secs: i64 },
    /// Committed packs with no record, all older than the epoch.
    Recordless { epoch: i64 },
}

/// One victim fence, one transaction (audit A7): `FOR UPDATE` the fence's
/// packs FIRST, re-read the full candidate predicate (audit A2 — a rerun may
/// have flipped the run non-terminal since nomination), the borrower check,
/// then delete the four row families. Answers whether the fence was expired;
/// any verdict short of that rolls back and leaves every row in place.
async fn expire_one_fence(
    db: &PgPool,
    fence_key: &str,
    victim: Victim,
    registry: &RetentionRegistry,
) -> Result<bool, sqlx::Error> {
    let mut tx = db.begin().await?;

    // 1 — LOCK the victim's pack rows before reading anything else. The drain
    // path's re-check holds `FOR SHARE OF p` on these same rows, so
    // record-first blocks this statement until the record AND its borrow
    // edges are committed (the borrower check below then sees the edge), and
    // expiry-first blocks the re-check until our deletion commits (the
    // re-driven drain then sees the absence and 422s).
    let locked: Vec<(String, bool, i64)> = sqlx::query_as(
        "SELECT pack_key, committed, created_at FROM depot_packs \
         WHERE fence_key = $1 FOR UPDATE",
    )
    .bind(fence_key)
    .fetch_all(&mut *tx)
    .await?;

    #[cfg(test)]
    {
        let inject = TEST_INJECT_IN_VICTIM_TXN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some((fence, sql)) = inject {
            if fence == fence_key {
                sqlx::query(&sql).execute(&mut *tx).await?;
            }
        }
    }

    // 2 — re-read the FULL candidate predicate inside the transaction.
    // Evaluated in code (fetch, then test) rather than as one EXISTS so the
    // S2 per-profile cutoff can be resolved per run without reshaping this.
    let still_a_victim = match victim {
        Victim::Recorded { now_secs } => {
            let run_row: Option<(String, i64, Option<i64>, Option<String>)> = sqlx::query_as(
                "SELECT ru.status, ru.updated_at, ru.snapshots_pinned_at, \
                        ru.ir->>'retention_profile' \
                 FROM depot_drain_records r \
                 JOIN runs ru ON ru.id = r.run \
                 WHERE r.fence_key = $1 AND r.record->>'error' IS NULL",
            )
            .bind(fence_key)
            .fetch_optional(&mut *tx)
            .await?;
            match run_row {
                Some((status, updated_at, pinned_at, profile)) => {
                    let cutoff = cutoff_ms(now_secs, registry.pack_ttl_secs(profile.as_deref()));
                    is_terminal_status(&status) && updated_at < cutoff && pinned_at.is_none()
                }
                // Record or run gone since nomination — not ours to judge.
                None => false,
            }
        }
        Victim::Recordless { epoch } => {
            let has_record: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM depot_drain_records WHERE fence_key = $1)",
            )
            .bind(fence_key)
            .fetch_one(&mut *tx)
            .await?;
            // A record appeared (a re-driven drain): the fence is a recorded
            // fence now, re-nominated through the recorded arm or not at all.
            !has_record
                && !locked.is_empty()
                && locked
                    .iter()
                    .all(|(_, committed, created_at)| *committed && *created_at < epoch)
        }
    };
    if !still_a_victim {
        tx.rollback().await?;
        return Ok(false);
    }

    // 3 — the borrower check, under the locks: no borrow edge on F may have a
    // borrower whose drain record still lives. F's own outbound edges do not
    // pin F — they die with F below, transitively freeing F's owners on a
    // later pass.
    let borrowed: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
             SELECT 1 FROM depot_fence_borrows b \
             JOIN depot_drain_records dr ON dr.fence_key = b.borrower_fence \
             WHERE b.owner_fence = $1)",
    )
    .bind(fence_key)
    .fetch_one(&mut *tx)
    .await?;
    if borrowed {
        tx.rollback().await?;
        return Ok(false);
    }

    // 4 — POINTERS ONLY, members first, packs keyed by the exact locked set.
    let pack_keys: Vec<String> = locked.into_iter().map(|(k, _, _)| k).collect();
    sqlx::query("DELETE FROM depot_pack_members WHERE pack_key = ANY($1)")
        .bind(&pack_keys)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM depot_packs WHERE fence_key = $1")
        .bind(fence_key)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM depot_drain_records WHERE fence_key = $1")
        .bind(fence_key)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM depot_fence_borrows WHERE borrower_fence = $1")
        .bind(fence_key)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    crate::metrics::record_depot_fence_expired();
    tracing::info!(
        fence_key = %fence_key,
        packs = pack_keys.len(),
        "depot expiry: expired a committed fence — rows gone, bytes rowless for \
         the orphan reclaimer (git-bug 6499fb1)"
    );
    Ok(true)
}

/// Whether an sqlx error is a Postgres deadlock (SQLSTATE `40P01`) — the one
/// error class that aborts the pass rather than failing it.
fn is_deadlock(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "40P01")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(
        name: &str,
        default: bool,
        pack_ttl_days: Option<u32>,
        workspace_ttl_days: Option<u32>,
    ) -> scarab_pipeline::RetentionProfile {
        scarab_pipeline::RetentionProfile {
            name: name.into(),
            default,
            pack_ttl_days,
            log_ttl_days: None,
            artifact_ttl_days: None,
            workspace_ttl_days,
        }
    }

    fn flat() -> ExpiryTtls {
        ExpiryTtls {
            pack_ttl_secs: days_secs(14),
            workspace_ttl_secs: days_secs(7),
        }
    }

    /// The shared registry validation, exercised through this consumer:
    /// duplicate names, two defaults and empty names each refuse the
    /// registry (= refuse the boot; the file is the operator's to fix).
    #[test]
    fn an_invalid_registry_refuses_the_boot() {
        for (profiles, why) in [
            (
                vec![profile("a", true, None, None), profile("a", false, None, None)],
                "duplicate name",
            ),
            (
                vec![profile("a", true, None, None), profile("b", true, None, None)],
                "two defaults",
            ),
            (vec![profile("", false, None, None)], "empty name"),
        ] {
            assert!(
                RetentionRegistry::new(profiles, flat()).is_err(),
                "{why} must be refused"
            );
        }
        // And the per-profile floor: a pack TTL below the resolved workspace
        // TTL would unback durable materialization while roots are reachable.
        let err = RetentionRegistry::new(vec![profile("thin", false, Some(2), Some(9))], flat())
            .expect_err("pack < workspace must refuse");
        assert!(err.contains("thin"), "the refusal names the profile: {err}");
    }

    /// Resolution: named profile wins; an UNKNOWN name falls to the default
    /// profile (warn — the registry is current truth, renames apply
    /// retroactively); no name falls to the default profile; absent fields
    /// and an empty registry fall to the flat env TTLs.
    #[test]
    fn resolution_falls_back_unknown_and_absent_to_default_then_flat() {
        let reg = RetentionRegistry::new(
            vec![
                profile("short", false, Some(9), None),
                profile("keep", true, Some(90), Some(30)),
            ],
            flat(),
        )
        .expect("valid registry");
        assert_eq!(reg.pack_ttl_secs(Some("short")), days_secs(9));
        assert_eq!(reg.pack_ttl_secs(Some("keep")), days_secs(90));
        assert_eq!(reg.pack_ttl_secs(Some("nope")), days_secs(90), "unknown → default");
        assert_eq!(reg.pack_ttl_secs(None), days_secs(90), "unnamed → default");
        // `short` leaves workspace unset → flat 7d backs its floor check.
        assert_eq!(reg.ttls_of(Some("short")).workspace_ttl_secs, days_secs(7));
        // The nomination CASE arms: every profile with its resolved pack TTL
        // (`short` leaves it to itself, `keep` to itself) — and the floor's
        // bound stays the most conservative workspace TTL.
        assert_eq!(
            reg.profile_pack_ttls(),
            vec![("short", days_secs(9)), ("keep", days_secs(90))]
        );
        assert_eq!(reg.max_workspace_ttl_secs(), days_secs(30));

        let bare = RetentionRegistry::flat(flat());
        assert_eq!(bare.pack_ttl_secs(Some("anything")), days_secs(14));
        assert_eq!(bare.pack_ttl_secs(None), days_secs(14));
    }

    /// [`TERMINAL_RUN_STATUSES_SQL`] and [`is_terminal_status`] against the
    /// domain's own `RunStatus::is_terminal`, exhaustively — a new run state
    /// added to the enum fails the match below at compile time, and a
    /// spelling drift fails the assertions.
    #[test]
    fn terminal_set_matches_the_domain() {
        use scarab_engine::RunStatus;
        // The `run_status_str` spellings (scarab-db-postgres) — the strings
        // the `runs.status` column actually holds.
        let all = [
            (RunStatus::Pending, "pending"),
            (RunStatus::Running, "running"),
            (RunStatus::Suspended, "suspended"),
            (RunStatus::Succeeded, "succeeded"),
            (RunStatus::Failed, "failed"),
            (RunStatus::Cancelled, "cancelled"),
            (RunStatus::DeadLettered, "dead_lettered"),
        ];
        for (status, spelling) in all {
            // Exhaustiveness: adding a RunStatus variant breaks this match.
            match status {
                RunStatus::Pending
                | RunStatus::Running
                | RunStatus::Suspended
                | RunStatus::Succeeded
                | RunStatus::Failed
                | RunStatus::Cancelled
                | RunStatus::DeadLettered => {}
            }
            assert_eq!(
                is_terminal_status(spelling),
                status.is_terminal(),
                "{spelling} disagrees with RunStatus::is_terminal"
            );
            assert_eq!(
                TERMINAL_RUN_STATUSES_SQL.contains(&format!("'{spelling}'")),
                status.is_terminal(),
                "the SQL IN-list disagrees with RunStatus::is_terminal on {spelling}"
            );
        }
    }

    /// The units guard (audit f4): `runs.updated_at` is MILLIS while the
    /// clock and TTLs are SECONDS — [`cutoff_ms`] is the one conversion, and
    /// a seconds-for-millis slip makes the cutoff land in 1970 (everything a
    /// victim) or year ~52000 (nothing ever expires).
    #[test]
    fn the_cutoff_converts_a_seconds_clock_and_ttl_into_millis() {
        let now_secs = 1_700_000_000; // one clock: Postgres now(), seconds
        let ttl_secs = 14 * 24 * 60 * 60;
        let cutoff = cutoff_ms(now_secs, ttl_secs);
        assert_eq!(cutoff, (now_secs - ttl_secs) * 1000);
        // A run that went terminal one hour before the TTL boundary (stored
        // in millis, like every `runs` timestamp) is past the cutoff; one an
        // hour after is not.
        let older_ms = (now_secs - ttl_secs - 3600) * 1000;
        let newer_ms = (now_secs - ttl_secs + 3600) * 1000;
        assert!(older_ms < cutoff, "past-TTL run must be a candidate");
        assert!(newer_ms >= cutoff, "within-TTL run must not be");
        // The failure modes the conversion exists to prevent: comparing the
        // millis column against a bare seconds cutoff would classify EVERY
        // terminal run as past-TTL.
        assert!(
            older_ms > now_secs - ttl_secs,
            "a millis timestamp dwarfs a seconds cutoff — the slip this guards"
        );
    }
}
