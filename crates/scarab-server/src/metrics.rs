//! Process-lifetime **counters** behind `GET /metrics` (ADR-0053).
//!
//! The gauges on `/metrics` are read live from the durable store on each scrape,
//! which is the right shape for *state* — runs by status, outbox depth — but it
//! cannot express an **event that already happened and left no distinguishing
//! state**. A commit-status post the forge rejected leaves its outbox row
//! looking exactly like one that simply hasn't been tried yet, so the backlog
//! gauge alone can't tell "busy" from "permanently broken" (ba921db: a GitHub
//! App missing `statuses:write` looked like nothing was happening at all).
//!
//! Those get monotonic process-global counters — the same shape every Prometheus
//! client library uses. Global rather than threaded through state because the
//! increments happen in the converged driver's outbox drain, which has no
//! `AppState`; they reset on restart, as counters are expected to, and `rate()`
//! over them is the alertable signal ("Scarab can't post statuses").

use std::sync::atomic::{AtomicU64, Ordering};

/// Commit-status posts the forge rejected (any `ForgePort::set_status` error).
static FORGE_STATUS_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Commit-status outbox messages retired as poison after `MAX_DELIVERY_ATTEMPTS`
/// — each one is a status that will NEVER reach the forge.
static FORGE_STATUS_DEAD_LETTERED: AtomicU64 = AtomicU64::new(0);

/// Count one rejected commit-status post.
pub fn record_forge_status_failure() {
    FORGE_STATUS_FAILURES.fetch_add(1, Ordering::Relaxed);
}

/// Count one commit-status message dead-lettered as poison.
pub fn record_forge_status_dead_lettered() {
    FORGE_STATUS_DEAD_LETTERED.fetch_add(1, Ordering::Relaxed);
}

/// Rejected commit-status posts since process start.
pub fn forge_status_failures() -> u64 {
    FORGE_STATUS_FAILURES.load(Ordering::Relaxed)
}

/// Commit-status messages dead-lettered since process start.
pub fn forge_status_dead_lettered() -> u64 {
    FORGE_STATUS_DEAD_LETTERED.load(Ordering::Relaxed)
}

/// Append the counters to a Prometheus text exposition body.
pub(crate) fn render(out: &mut String) {
    out.push_str(&format!(
        "# HELP scarab_forge_status_failures_total Commit-status posts rejected by the forge.
# TYPE scarab_forge_status_failures_total counter
scarab_forge_status_failures_total {}
# HELP scarab_forge_status_dead_lettered_total Commit-status messages retired as poison (never posted).
# TYPE scarab_forge_status_dead_lettered_total counter
scarab_forge_status_dead_lettered_total {}
",
        forge_status_failures(),
        forge_status_dead_lettered()
    ));
    // The control plane's own view of the ADR-0061 tiering. The workspace
    // *service* exports the same counters from its own `/metrics`, and both are
    // wanted: these say what the CONTROL PLANE saw of the service (writes it
    // could not seed, reads it had to serve from cold storage directly), which is
    // the only place a service that is up-but-unreachable-from-here shows up at
    // all.
    {
        use scarab_storage::tiered;
        out.push_str(&format!(
            "# HELP scarab_workspace_warm_write_failed_total Snapshot writes that reached cold but not the workspace service (durable; a cache miss to come).
# TYPE scarab_workspace_warm_write_failed_total counter
scarab_workspace_warm_write_failed_total {}
# HELP scarab_workspace_cold_fallback_total Snapshot reads served from cold storage because the workspace service did not have them.
# TYPE scarab_workspace_cold_fallback_total counter
scarab_workspace_cold_fallback_total {}
# HELP scarab_workspace_warm_read_failed_total Snapshot reads where the workspace service ERRORED and cold storage answered instead (ADR-0061 D1.6).
# TYPE scarab_workspace_warm_read_failed_total counter
scarab_workspace_warm_read_failed_total {}
",
            tiered::warm_write_failed_total(),
            tiered::cold_fallback_total(),
            tiered::warm_read_failed_total(),
        ));
    }
}
