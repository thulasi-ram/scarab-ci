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

use scarab_engine::{
    Clock, Db, Executor, Scheduler, SchedulerError, ServiceStatus, StepStatus, Supervision,
};
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
    public_url: &str,
    supervision: &Supervision,
) -> Result<(), SchedulerError> {
    // The Scheduler is per-cycle; the Supervision memory is per-PROCESS
    // (ADR-0056) — that is what lets a resumed control plane recognise the
    // attempts it did NOT launch and emit `AttemptReadopted` exactly once.
    Scheduler::new(&**db, &**clock, &**executor, owner)
        .with_outbox_visibility_ms(visibility_ms)
        .with_default_step_timeout_ms(step_timeout_ms)
        .with_supervision(supervision.clone())
        .tick_all()
        .await?;
    // Log tail (ADR-0013): pull each running step's stdout/stderr into the log
    // pipeline. Best-effort and idempotent per fence — the tailer dedups, so
    // re-ensuring every tick just no-ops for streams already in flight.
    if let Some(tailer) = tailer {
        if let Err(e) = ensure_log_tails(db, tailer).await {
            tracing::warn!(error = %e, "ensuring log tails failed");
        }
        // Shared-service log tails (ADR-0058 evidence): same best-effort channel,
        // keyed on the service instance instead of a step fence.
        if let Err(e) = ensure_service_tails(db, tailer).await {
            tracing::warn!(error = %e, "ensuring service log tails failed");
        }
    }
    if let Some(forge) = forge {
        // Status posting is best-effort within a tick; a failed post stays on the
        // outbox for the next cycle (at-least-once, idempotent).
        if let Err(e) =
            crate::drain_forge_statuses(&**forge, &**db, owner, 32, 30_000, public_url).await
        {
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

/// Ensure a best-effort log tail for every **live shared service** of the
/// current Take across all active runs (ADR-0058). Only the current Take's
/// instances are tailed (a Rerun's prior Take is being torn down); an instance
/// without a launch handle, or not yet ready, is skipped and picked up a later
/// tick. Idempotent per instance — the tailer dedups.
async fn ensure_service_tails(db: &Arc<dyn Db>, tailer: &LogTailer) -> Result<(), SchedulerError> {
    for run in db.active_runs().await? {
        let services = db.run_services(&run).await?;
        let Some(current_take) = services.iter().map(|s| s.take).max() else {
            continue;
        };
        for s in services {
            if s.take != current_take {
                continue;
            }
            // A tail needs a launched Pod; readiness is when the container is up
            // and producing logs. `Starting` has no Pod-log yet; terminal states
            // have nothing more to add.
            if !matches!(s.status, ServiceStatus::Ready | ServiceStatus::Running) {
                continue;
            }
            if let Some(handle) = &s.handle {
                tailer.ensure_service(&run, s.take, &s.name, handle);
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
    public_url: String,
) -> tokio::task::JoinHandle<()> {
    // Claim-to-tail lease (ADR-0051): with 2+ replicas, only the fence's
    // lease holder tails a step — deduping ingestion and spreading log I/O.
    let tailer = logs
        .map(|logs| LogTailer::new(executor.clone(), logs).with_lease(db.clone(), owner.clone()));
    // One Supervision per driver process (ADR-0056) — see `tick_once`.
    let supervision = Supervision::new();
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
                &public_url,
                &supervision,
            )
            .await
            {
                tracing::warn!(error = %e, "converged driver tick failed");
            }
            tokio::time::sleep(interval).await;
        }
    })
}
