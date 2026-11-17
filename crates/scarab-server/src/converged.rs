//! Converged in-process driver (ADR-0016): run the scheduler + executor roles as
//! a background loop in the same process as the API. Postgres (the outbox) is the
//! coordination bus, so "scale out" later is just running these roles as separate
//! replicas — no code change.
//!
//! The loop repeatedly ticks the pure [`Scheduler`] across every active run:
//! admit ready work → reconcile launch intents on the executor → advance runs to
//! terminal. Each tick reads durable state, so it is crash-safe and idempotent.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
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
    project_run_cap: Option<u32>,
    public_url: &str,
    supervision: &Supervision,
    health: &TickHealth,
) -> Result<(), SchedulerError> {
    tick_cycle(
        &DriverHealth::new(0),
        db,
        clock,
        executor,
        snapshots,
        forge,
        tailer,
        observer,
        owner,
        visibility_ms,
        step_timeout_ms,
        project_run_cap,
        public_url,
        supervision,
        health,
    )
    .await
}

/// [`tick_once`], recording in `driver` which part of the cycle runs (so a
/// cycle cut off at its deadline can say where it stalled) and whether this
/// replica holds the scheduler lease (git-bug afa9f07).
#[allow(clippy::too_many_arguments)]
async fn tick_cycle(
    driver: &DriverHealth,
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
    project_run_cap: Option<u32>,
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
    driver.enter("scheduler tick");
    let mut scheduler =
        Scheduler::new(&**db, &**clock, &**executor, owner).with_leader_flag(driver.leader_flag());
    if let Some(cap) = project_run_cap {
        scheduler = scheduler.with_project_run_cap(cap);
    }
    let isolated = scheduler
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
        driver.enter("log tails");
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
        driver.enter("infra observation");
        if let Err(e) = observe_infra(db, observer).await {
            tracing::warn!(error = %e, "infra observation failed");
        }
    }
    if let Some(forge) = forge {
        // Status posting is best-effort within a tick; a failed post stays on the
        // outbox for the next cycle (at-least-once, idempotent).
        driver.enter("forge status drain");
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
    project_run_cap: Option<u32>,
    public_url: String,
    driver: Arc<DriverHealth>,
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
    let ctx = Arc::new(CycleInputs {
        driver: driver.clone(),
        db,
        clock: clock.clone(),
        executor,
        snapshots,
        forge,
        tailer,
        observer,
        owner,
        visibility_ms,
        step_timeout_ms,
        project_run_cap,
        public_url,
        supervision,
        health,
    });
    let looping = tokio::spawn(async move {
        drive(&driver, &clock, interval, CYCLE_DEADLINE, move || {
            let ctx = ctx.clone();
            async move { ctx.run().await }
        })
        .await
    });
    // A driver that stops (a panic, or the loop returning) leaves runs
    // pending with nothing logged (git-bug afa9f07): end the process, so
    // Kubernetes starts a new one. Aborting the returned handle at shutdown
    // aborts the loop with it and exits nothing.
    tokio::spawn(supervise(looping, |why| {
        tracing::error!(
            why = %why,
            "the converged scheduler driver stopped; exiting so the process restarts"
        );
        std::process::exit(1);
    }))
}

/// Everything one driver cycle reads, shared by every cycle.
struct CycleInputs {
    driver: Arc<DriverHealth>,
    db: Arc<dyn Db>,
    clock: Arc<dyn Clock>,
    executor: Arc<dyn Executor>,
    snapshots: Option<Arc<dyn WorkspaceSnapshots>>,
    forge: Option<Arc<dyn ForgePort>>,
    tailer: Option<LogTailer>,
    observer: InfraObserver,
    owner: String,
    visibility_ms: i64,
    step_timeout_ms: i64,
    project_run_cap: Option<u32>,
    public_url: String,
    supervision: Supervision,
    health: TickHealth,
}

impl CycleInputs {
    async fn run(&self) -> Result<(), SchedulerError> {
        tick_cycle(
            &self.driver,
            &self.db,
            &self.clock,
            &self.executor,
            self.snapshots.as_ref(),
            self.forge.as_ref(),
            self.tailer.as_ref(),
            Some(&self.observer),
            &self.owner,
            self.visibility_ms,
            self.step_timeout_ms,
            self.project_run_cap,
            &self.public_url,
            &self.supervision,
            &self.health,
        )
        .await
    }
}

/// How long one driver cycle may take before it is cut off (git-bug
/// afa9f07). Twice the scheduler lease TTL: a cycle over many runs fits, and
/// a cycle stuck on a call that never returns no longer holds the loop.
pub const CYCLE_DEADLINE: Duration = Duration::from_secs(60);

/// How long a replica that holds the scheduler lease may go without
/// finishing a cycle before `/livez` fails (git-bug afa9f07): three lease
/// TTLs, so one cycle cut off at [`CYCLE_DEADLINE`] does not fail it and a
/// second one does.
pub const STALLED_AFTER_MS: i64 = 90_000;

/// What the converged driver is doing (git-bug afa9f07): when it last
/// finished a cycle, which part of the cycle runs now, and whether this
/// replica held the scheduler lease when it last asked for it. `/livez`
/// reads it.
#[derive(Debug)]
pub struct DriverHealth {
    last_cycle_ms: AtomicI64,
    leader: Arc<AtomicBool>,
    leg: Mutex<&'static str>,
}

impl DriverHealth {
    /// A driver that has not run yet counts as having finished a cycle at
    /// `now_ms`, so a process that just started is live.
    pub fn new(now_ms: i64) -> Self {
        DriverHealth {
            last_cycle_ms: AtomicI64::new(now_ms),
            leader: Arc::new(AtomicBool::new(false)),
            leg: Mutex::new("start"),
        }
    }

    fn enter(&self, leg: &'static str) {
        *self.leg.lock().unwrap_or_else(|p| p.into_inner()) = leg;
    }

    fn leg(&self) -> &'static str {
        *self.leg.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn finished(&self, now_ms: i64) {
        self.last_cycle_ms.store(now_ms, Ordering::Relaxed);
    }

    /// The flag the scheduler sets each time it asks for its lease.
    pub fn leader_flag(&self) -> Arc<AtomicBool> {
        self.leader.clone()
    }

    /// Why the driver counts as stalled at `now_ms`, or `None`: this
    /// replica held the scheduler lease when it last asked, and no cycle has
    /// finished for more than `stalled_after_ms`. A replica that does not
    /// lead is never stalled here: another replica runs the scheduler.
    pub fn stalled(&self, now_ms: i64, stalled_after_ms: i64) -> Option<String> {
        if !self.leader.load(Ordering::Relaxed) {
            return None;
        }
        let idle_ms = now_ms.saturating_sub(self.last_cycle_ms.load(Ordering::Relaxed));
        (idle_ms > stalled_after_ms).then(|| {
            format!(
                "the scheduler driver holds the scheduler lease and has not finished a cycle \
                 for {}s (last in: {})",
                idle_ms / 1000,
                self.leg()
            )
        })
    }
}

/// Run `cycle` every `interval`, each run cut off at `deadline` (git-bug
/// afa9f07). A cycle that finishes, with or without an error, marks the
/// driver live; one cut off does not, and a WARN names the part it was in.
/// Never returns.
pub async fn drive<F>(
    driver: &DriverHealth,
    clock: &Arc<dyn Clock>,
    interval: Duration,
    deadline: Duration,
    mut cycle: impl FnMut() -> F,
) where
    F: std::future::Future<Output = Result<(), SchedulerError>>,
{
    loop {
        driver.enter("start");
        match tokio::time::timeout(deadline, cycle()).await {
            Ok(result) => {
                if let Err(e) = result {
                    tracing::warn!(error = %e, "converged driver tick failed");
                }
                driver.finished(clock.now().await.0);
            }
            Err(_) => tracing::warn!(
                part = driver.leg(),
                deadline_secs = deadline.as_secs(),
                "a converged driver cycle did not finish within its deadline; it was cut \
                 off and the next cycle starts"
            ),
        }
        tokio::time::sleep(interval).await;
    }
}

/// Wait for `task` to end, then call `on_end` with how it ended. Dropping
/// the returned future (an abort at shutdown) aborts `task` too.
pub async fn supervise(task: tokio::task::JoinHandle<()>, on_end: impl FnOnce(String)) {
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let mut task = AbortOnDrop(task);
    let why = match (&mut task.0).await {
        Ok(()) => "the loop returned".to_string(),
        Err(e) if e.is_panic() => {
            let payload = e.into_panic();
            let text = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "no message".to_string());
            format!("it panicked: {text}")
        }
        Err(e) => format!("it was cancelled: {e}"),
    };
    on_end(why);
}

#[cfg(test)]
mod driver_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn clock(at: i64) -> Arc<dyn Clock> {
        Arc::new(scarab_testkit::FakeClock::new(at))
    }

    /// git-bug afa9f07: a cycle that never returns is cut off at its
    /// deadline, the next cycle runs, and only a finished cycle marks the
    /// driver live.
    #[tokio::test(start_paused = true)]
    async fn a_cycle_that_hangs_is_cut_off_and_the_next_one_runs() {
        let driver = DriverHealth::new(0);
        let clock = clock(5_000);
        let runs = AtomicUsize::new(0);
        let looping = drive(
            &driver,
            &clock,
            Duration::from_millis(500),
            Duration::from_secs(60),
            || {
                let n = runs.fetch_add(1, Ordering::SeqCst);
                let driver = &driver;
                async move {
                    if n == 0 {
                        driver.enter("forge status drain");
                        std::future::pending::<()>().await;
                    }
                    Ok(())
                }
            },
        );
        tokio::select! {
            () = looping => unreachable!("drive never returns"),
            () = async {
                while runs.load(Ordering::SeqCst) < 2 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            } => {}
        }
        assert!(runs.load(Ordering::SeqCst) >= 2, "the second cycle ran");
        assert_eq!(
            driver.last_cycle_ms.load(Ordering::Relaxed),
            5_000,
            "it finished and counted"
        );
    }

    /// Liveness fails only for the lease holder whose cycles stopped.
    #[test]
    fn stalled_only_when_leader_and_no_cycle_finished_recently() {
        let driver = DriverHealth::new(1_000);
        assert_eq!(
            driver.stalled(1_000_000, STALLED_AFTER_MS),
            None,
            "not the leader"
        );
        driver.leader_flag().store(true, Ordering::Relaxed);
        assert_eq!(
            driver.stalled(1_000 + STALLED_AFTER_MS, STALLED_AFTER_MS),
            None,
            "at the bound"
        );
        driver.enter("scheduler tick");
        let why = driver
            .stalled(1_000 + STALLED_AFTER_MS + 1_000, STALLED_AFTER_MS)
            .expect("stalled");
        assert!(
            why.contains("91s") && why.contains("scheduler tick"),
            "{why}"
        );
        driver.finished(200_000);
        assert_eq!(
            driver.stalled(250_000, STALLED_AFTER_MS),
            None,
            "a cycle finished"
        );
    }

    /// A driver task that panics or returns is reported; aborting the
    /// supervisor aborts the task and reports nothing.
    #[tokio::test]
    async fn a_driver_that_stops_is_reported_and_an_abort_is_not() {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx2 = tx.clone();
        supervise(tokio::spawn(async { panic!("boom") }), move |why| {
            tx.send(why).unwrap()
        })
        .await;
        assert!(rx.recv().unwrap().contains("panicked: boom"));
        supervise(tokio::spawn(async {}), move |why| tx2.send(why).unwrap()).await;
        assert!(rx.recv().unwrap().contains("returned"));

        let (started_tx, started) = tokio::sync::oneshot::channel::<()>();
        let inner = tokio::spawn(async move {
            let _keep = started_tx;
            std::future::pending::<()>().await;
        });
        let watcher = tokio::spawn(supervise(inner, |_| panic!("an abort must not report")));
        tokio::task::yield_now().await;
        watcher.abort();
        assert!(watcher.await.unwrap_err().is_cancelled());
        // The inner task was aborted with it: its sender is dropped.
        assert!(started.await.is_err());
    }
}
