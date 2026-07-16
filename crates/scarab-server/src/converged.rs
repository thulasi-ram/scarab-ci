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

use scarab_engine::{Clock, Db, Executor, Scheduler, SchedulerError, StepStatus};
use scarab_forge::ForgePort;

use crate::LogTailer;

/// Run one converged cycle across all active runs: tick the scheduler, ensure a
/// live log tail is running for every in-flight step (if a `tailer` is wired),
/// then (if a `forge` is wired) post any pending commit statuses back.
// Wiring seam: all inputs are distinct composition-root dependencies (see
// `spawn_driver`); a config struct would only add indirection.
#[allow(clippy::too_many_arguments)]
pub async fn tick_once(
    db: &Arc<dyn Db>,
    clock: &Arc<dyn Clock>,
    executor: &Arc<dyn Executor>,
    forge: Option<&Arc<dyn ForgePort>>,
    tailer: Option<&LogTailer>,
    owner: &str,
    visibility_ms: i64,
    step_timeout_ms: i64,
) -> Result<(), SchedulerError> {
    Scheduler::new(&**db, &**clock, &**executor, owner)
        .with_outbox_visibility_ms(visibility_ms)
        .with_default_step_timeout_ms(step_timeout_ms)
        .tick_all()
        .await?;
    // Log tail (ADR-0013): pull each running step's stdout/stderr into the log
    // pipeline. Best-effort and idempotent per fence — the tailer dedups, so
    // re-ensuring every tick just no-ops for streams already in flight.
    if let Some(tailer) = tailer {
        if let Err(e) = ensure_log_tails(db, tailer).await {
            tracing::warn!(error = %e, "ensuring log tails failed");
        }
    }
    if let Some(forge) = forge {
        // Status posting is best-effort within a tick; a failed post stays on the
        // outbox for the next cycle (at-least-once, idempotent).
        if let Err(e) = crate::drain_forge_statuses(&**forge, &**db, owner, 32, 30_000).await {
            tracing::warn!(error = %e, "forge status drain failed");
        }
    }
    Ok(())
}

/// Ensure a log tail is running for every currently-running step across all
/// active runs. Reads durable state each tick, so it re-attaches a tail after a
/// control-plane restart (the tailer's dedup keeps it from starting twice).
async fn ensure_log_tails(db: &Arc<dyn Db>, tailer: &LogTailer) -> Result<(), SchedulerError> {
    for run in db.active_runs().await? {
        for step in db.steps_of_run(&run).await? {
            if step.status == StepStatus::Running {
                tailer.ensure(&step);
            }
        }
    }
    Ok(())
}

/// Spawn the background driver: tick, sleep `interval`, repeat. Returns the task
/// handle (abort it to stop). A failed tick is logged and retried next interval —
/// forward progress resumes from durable state.
///
/// When `logs` is supplied the driver also runs the log-tail source (ADR-0013):
/// each tick it ensures a best-effort tail into that [`LogService`] for every
/// running step, using the same `executor` as the launch path.
// Wiring seam: all inputs are distinct composition-root dependencies, so bundling
// them into a config struct would only add indirection.
#[allow(clippy::too_many_arguments)]
pub fn spawn_driver(
    db: Arc<dyn Db>,
    clock: Arc<dyn Clock>,
    executor: Arc<dyn Executor>,
    forge: Option<Arc<dyn ForgePort>>,
    logs: Option<Arc<crate::LogService>>,
    owner: String,
    interval: Duration,
    visibility_ms: i64,
    step_timeout_ms: i64,
) -> tokio::task::JoinHandle<()> {
    let tailer = logs.map(|logs| LogTailer::new(executor.clone(), logs));
    tokio::spawn(async move {
        loop {
            if let Err(e) = tick_once(
                &db,
                &clock,
                &executor,
                forge.as_ref(),
                tailer.as_ref(),
                &owner,
                visibility_ms,
                step_timeout_ms,
            )
            .await
            {
                tracing::warn!(error = %e, "converged driver tick failed");
            }
            tokio::time::sleep(interval).await;
        }
    })
}
