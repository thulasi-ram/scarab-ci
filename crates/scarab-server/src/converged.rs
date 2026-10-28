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
    TickHealth, WorkspaceSnapshots,
};
use scarab_forge::ForgePort;

use crate::{InfraObserver, LogTailer};

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
    snapshots: Option<&Arc<dyn WorkspaceSnapshots>>,
    forge: Option<&Arc<dyn ForgePort>>,
    tailer: Option<&LogTailer>,
    observer: Option<&InfraObserver>,
    owner: &str,
    visibility_ms: i64,
    step_timeout_ms: i64,
    public_url: &str,
    supervision: &Supervision,
    health: &TickHealth,
) -> Result<(), SchedulerError> {
    // The Scheduler is per-cycle; the Supervision memory is per-PROCESS
    // (ADR-0056) — that is what lets a resumed control plane recognise the
    // attempts it did NOT launch and emit `AttemptReadopted` exactly once.
    // TickHealth is per-PROCESS for the same reason (ADR-0059): it dates each
    // run's current failure streak, and a per-cycle map would never reach the
    // dead-letter bound.
    //
    // Per-run tick errors the engine isolated (ADR-0059; originally git-bug
    // 6825830 for reconcile_services alone) come back here — the pure engine
    // does not log; the driver, which owns tracing, surfaces them without
    // aborting the tick.
    let isolated = Scheduler::new(&**db, &**clock, &**executor, owner)
        // Cache-key resolution at launch (ADR-0065 s1) rides the same oracle
        // the rerun-widening legs use; `None` = the cache is silently off.
        .with_snapshots(snapshots.map(|s| &**s as &dyn WorkspaceSnapshots))
        .with_outbox_visibility_ms(visibility_ms)
        .with_default_step_timeout_ms(step_timeout_ms)
        .with_supervision(supervision.clone())
        .with_tick_health(health.clone())
        .tick_all()
        .await?;
    for (run, e) in &isolated {
        tracing::warn!(
            run = %run.0, error = %e,
            "per-run tick leg failed; run skipped this cycle, retried next \
             (dead-letters if it keeps failing — ADR-0059)"
        );
    }
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
    // Infra observation (ADR-0068): narrate why an in-flight step has no logs
    // yet. Runs on its own 30s cadence inside the observer, so calling it every
    // tick is cheap; best-effort, and it never influences a verdict.
    if let Some(observer) = observer {
        if let Err(e) = observe_infra(db, observer).await {
            tracing::warn!(error = %e, "infra observation failed");
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
                // Also tail each authored sidecar service (ADR-0058), co-located in
                // the step's Pod as container `service-{i}`. Enumerating the step
                // spec's `services:` and addressing `service-{i}` by index
                // naturally excludes the framework sidecars (results-egress
                // `scarab-results-egress`, workspace `scarab-workspace-*`), which
                // carry distinct names — so their output never mixes into a step's
                // sidecar streams. Best-effort + idempotent per fence (the tailer
                // dedups), like `ensure` itself; a step with no stored spec (a gate)
                // has no services.
                if let Some(spec) = db.step_spec(&run, &step.step).await? {
                    for i in 0..spec.services.len() {
                        tailer.ensure_sidecar(&step, i);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Observe the backend condition of every in-flight step across all active runs,
/// then retire the fences that are no longer in flight (ADR-0068).
///
/// The retire pass is not bookkeeping — it is what emits the closing event for a
/// step that ended *while* wedged, which is the usual way a wedged step ends: the
/// retry budget or the timeout kills it rather than it recovering. Computing the
/// live set from the same pass that observed keeps the two consistent within a
/// tick.
async fn observe_infra(db: &Arc<dyn Db>, observer: &InfraObserver) -> Result<(), SchedulerError> {
    let mut live = Vec::new();
    for run in db.active_runs().await? {
        for step in db.steps_of_run(&run).await? {
            if step.status == StepStatus::Running {
                if let Err(e) = observer.observe(&step).await {
                    tracing::warn!(
                        run = %run.0, step = %step.step.0, error = %e,
                        "observing infra condition failed; retried next cycle"
                    );
                }
                live.push(step);
            }
        }
    }
    if let Err(e) = observer.retire(&crate::live_fences(&live)).await {
        tracing::warn!(error = %e, "retiring infra observations failed");
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
    snapshots: Option<Arc<dyn WorkspaceSnapshots>>,
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
    // Infra observation (ADR-0068) is unconditional: unlike the log tail it needs
    // no object store, and the failures it exists to explain are exactly the ones
    // that produce no logs to store.
    let observer = InfraObserver::new(executor.clone(), db.clone(), clock.clone());
    // One Supervision per driver process (ADR-0056) — see `tick_once`.
    let supervision = Supervision::new();
    // Likewise one TickHealth per driver process (ADR-0059).
    let health = TickHealth::new();
    tokio::spawn(async move {
        loop {
            if let Err(e) = tick_once(
                &db,
                &clock,
                &executor,
                snapshots.as_ref(),
                forge.as_ref(),
                tailer.as_ref(),
                Some(&observer),
                &owner,
                visibility_ms,
                step_timeout_ms,
                &public_url,
                &supervision,
                &health,
            )
            .await
            {
                tracing::warn!(error = %e, "converged driver tick failed");
            }
            tokio::time::sleep(interval).await;
        }
    })
}
