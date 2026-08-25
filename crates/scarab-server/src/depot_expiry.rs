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

/// The class TTLs the pass needs, in seconds (S1: the flat env knobs;
/// RetentionProfile resolution replaces this as the per-run source in S2).
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
    ttls: ExpiryTtls,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match expire_committed_fences_once(&db, &ttls, EXPIRY_BATCH).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    fences = n,
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
    ttls: &ExpiryTtls,
    batch: u32,
) -> Result<u32, sqlx::Error> {
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
            "depot expiry: another replica (or the Depot reclaimer) holds the pass \
             lock — not a skip, the work is happening elsewhere"
        );
        return Ok(0);
    }

    let result = expire_pass(db, ttls, batch).await;

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
    result
}

/// The pass body, under the advisory lock: the floor, then the recorded arm,
/// then (floor permitting) the recordless arm.
async fn expire_pass(db: &PgPool, ttls: &ExpiryTtls, batch: u32) -> Result<u32, sqlx::Error> {
    // One clock authority: Postgres `now()`, read beside the epoch (stamped
    // once, from that same clock, when migration 0048 ran).
    let (now_secs, epoch): (i64, i64) = sqlx::query_as(
        "SELECT EXTRACT(EPOCH FROM now())::bigint, epoch FROM depot_borrow_tracking_epoch",
    )
    .fetch_one(db)
    .await?;
    let pack_cutoff = cutoff_ms(now_secs, ttls.pack_ttl_secs);
    let ws_cutoff = cutoff_ms(now_secs, ttls.workspace_ttl_secs);
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

    // --- The recorded arm: success records whose run is terminal, past the
    // pack TTL and unpinned. `pre_epoch` (any pack older than the epoch) is
    // what scopes the floor: a post-epoch fence's borrows are all recorded,
    // so it expires on edges alone even while the floor is up.
    let candidates: Vec<(String, bool)> = sqlx::query_as(&format!(
        "SELECT r.fence_key, \
                EXISTS (SELECT 1 FROM depot_packs p \
                        WHERE p.fence_key = r.fence_key AND p.created_at < $3) AS pre_epoch \
         FROM depot_drain_records r \
         JOIN runs ru ON ru.id = r.run \
         WHERE r.record->>'error' IS NULL \
           AND ru.status IN {TERMINAL_RUN_STATUSES_SQL} \
           AND ru.updated_at < $1 \
           AND ru.snapshots_pinned_at IS NULL \
         ORDER BY r.posted_at \
         LIMIT $2"
    ))
    .bind(pack_cutoff)
    .bind(i64::from(batch))
    .bind(epoch)
    .fetch_all(db)
    .await?;
    for (fence, pre_epoch) in candidates {
        if pre_epoch && floor_held {
            tracing::debug!(
                fence_key = %fence,
                "depot expiry: pre-epoch victim held by the reachability floor — a \
                 pre-epoch run could still silently borrow from it"
            );
            continue;
        }
        match expire_one_fence(db, &fence, Victim::Recorded { cutoff_ms: pack_cutoff }).await {
            Ok(true) => expired += 1,
            Ok(false) => {}
            Err(e) if is_deadlock(&e) => {
                crate::metrics::record_depot_expiry_pass_skipped();
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
            match expire_one_fence(db, &fence, Victim::Recordless { epoch }).await {
                Ok(true) => expired += 1,
                Ok(false) => {}
                Err(e) if is_deadlock(&e) => {
                    crate::metrics::record_depot_expiry_pass_skipped();
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

    Ok(expired)
}

/// Which arm nominated a victim — and therefore which predicate the victim
/// transaction re-reads before deleting.
enum Victim {
    /// A live success record whose run answered the candidate query.
    Recorded { cutoff_ms: i64 },
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
        Victim::Recorded { cutoff_ms } => {
            let run_row: Option<(String, i64, Option<i64>)> = sqlx::query_as(
                "SELECT ru.status, ru.updated_at, ru.snapshots_pinned_at \
                 FROM depot_drain_records r \
                 JOIN runs ru ON ru.id = r.run \
                 WHERE r.fence_key = $1 AND r.record->>'error' IS NULL",
            )
            .bind(fence_key)
            .fetch_optional(&mut *tx)
            .await?;
            match run_row {
                Some((status, updated_at, pinned_at)) => {
                    is_terminal_status(&status) && updated_at < cutoff_ms && pinned_at.is_none()
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
