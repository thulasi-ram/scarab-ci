//! Converged in-process driver (ADR-0016): run the scheduler + executor roles as
//! a background loop in the same process as the API. Postgres (the outbox) is the
//! coordination bus, so "scale out" later is just running these roles as separate
//! replicas — no code change.
//!
//! The loop repeatedly ticks the pure [`Scheduler`] across every active run:
//! admit ready work → reconcile launch intents on the executor → advance runs to
//! terminal. Each tick reads durable state, so it is crash-safe and idempotent.

use std::sync::Arc;
use std::time::Duration;

use scarab_engine::{Clock, Db, Executor, Scheduler, SchedulerError};
use scarab_forge::ForgePort;

/// Run one converged cycle across all active runs: tick the scheduler, then (if a
/// `forge` is wired) post any pending commit statuses back.
pub async fn tick_once(
    db: &Arc<dyn Db>,
    clock: &Arc<dyn Clock>,
    executor: &Arc<dyn Executor>,
    forge: Option<&Arc<dyn ForgePort>>,
    owner: &str,
) -> Result<(), SchedulerError> {
    Scheduler::new(&**db, &**clock, &**executor, owner)
        .tick_all()
        .await?;
    if let Some(forge) = forge {
        // Status posting is best-effort within a tick; a failed post stays on the
        // outbox for the next cycle (at-least-once, idempotent).
        if let Err(e) = crate::drain_forge_statuses(&**forge, &**db, owner, 32, 30_000).await {
            tracing::warn!(error = %e, "forge status drain failed");
        }
    }
    Ok(())
}

/// Spawn the background driver: tick, sleep `interval`, repeat. Returns the task
/// handle (abort it to stop). A failed tick is logged and retried next interval —
/// forward progress resumes from durable state.
pub fn spawn_driver(
    db: Arc<dyn Db>,
    clock: Arc<dyn Clock>,
    executor: Arc<dyn Executor>,
    forge: Option<Arc<dyn ForgePort>>,
    owner: String,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = tick_once(&db, &clock, &executor, forge.as_ref(), &owner).await {
                tracing::warn!(error = %e, "converged driver tick failed");
            }
            tokio::time::sleep(interval).await;
        }
    })
}
