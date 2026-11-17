//! Converged in-process driver (ADR-0016): run the scheduler + executor roles as
//! a background loop in the same process as the API. Postgres (the outbox) is the
//! coordination bus, so "scale out" later is just running these roles as separate
//! replicas — no code change.
//!
//! The loop repeatedly ticks the pure [`Scheduler`] across every active run:
//! admit ready work → reconcile launch intents on the executor → advance runs to
//! terminal. Each tick reads durable state, so it is crash-safe and idempotent.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use scarab_engine::{
    Clock, Db, Executor, LeaderPulse, Scheduler, SchedulerError, ServiceStatus, StepStatus,
    Supervision, TickHealth, WorkspaceSnapshots,
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
        &DriverHealth::new(),
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
        Scheduler::new(&**db, &**clock, &**executor, owner).with_leader_pulse(driver.pulse());
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
        let stall_cutoff = driver.stall_cutoff();
        drive(&driver, interval, stall_cutoff, move || {
            let ctx = ctx.clone();
            async move { ctx.run().await }
        })
        .await
    });
    // A driver that stops (a panic, or the loop returning) leaves runs
    // pending with nothing logged (git-bug afa9f07): end the process, so
    // Kubernetes starts a new one. Aborting the returned handle at shutdown
    // aborts the loop with it and exits nothing.
    // A driver that stops (a panic, or the loop returning) leaves runs
    // pending with nothing logged (git-bug afa9f07): shut the process down
    // through its graceful path, which then exits non-zero, so Kubernetes
    // starts a new one. Aborting the returned handle at shutdown aborts the
    // loop with it and signals nothing.
    tokio::spawn(supervise(looping, |why| {
        tracing::error!(
            why = %why,
            "the converged scheduler driver stopped; shutting down so the process restarts"
        );
        signal_driver_stopped();
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

/// How long a driver cycle may go without progress before it is cut off
/// (git-bug afa9f07), unless `SCARAB_DRIVER_STALL_CUTOFF_SECS` says
/// otherwise. Progress is the scheduler asking for its lease (it does for
/// each Run's leader legs) or the cycle entering its next part, so a slow
/// cycle over many Runs is never cut; one stuck on a call that never
/// returns is.
pub const STALL_CUTOFF: Duration = Duration::from_secs(60);

/// How long a replica's driver may go without progress before `/livez`
/// fails (git-bug afa9f07), unless `SCARAB_DRIVER_STALLED_AFTER_SECS` says
/// otherwise: three lease TTLs, so a stuck cycle is cut off at
/// [`STALL_CUTOFF`] first, and a driver that is still stuck after that is
/// restarted.
pub const STALLED_AFTER: Duration = Duration::from_secs(90);

/// The smallest stall cutoff a setting may ask for: one scheduler lease
/// TTL. A working leader asks for its lease more often than this.
pub const MIN_STALL_CUTOFF_SECS: u64 = 30;

/// How much longer than the stall cutoff `/livez` must wait: one lease TTL,
/// so a cycle that is cut off and starts again is not also restarted.
pub const MIN_STALLED_AFTER_MARGIN_SECS: u64 = 30;

/// The largest either limit may be: an hour.
pub const MAX_DRIVER_LIMIT_SECS: u64 = 3_600;

/// How often the driver checks a running cycle for progress.
const PROGRESS_CHECK: Duration = Duration::from_secs(1);

/// What the converged driver is doing (git-bug afa9f07): when it last made
/// progress, which part of the cycle runs now, whether this replica held
/// the scheduler lease when it last asked for it, and the two limits it is
/// held to. `/livez` reads it.
/// Times are monotonic, so a wall-clock step neither hides a stall nor
/// fakes one.
#[derive(Debug)]
pub struct DriverHealth {
    pulse: Arc<LeaderPulse>,
    leg: Mutex<&'static str>,
    legs_entered: AtomicU64,
    progress: Mutex<Progress>,
    stall_cutoff: Duration,
    stalled_after: Duration,
}

#[derive(Debug)]
struct Progress {
    /// Lease asks plus parts entered, when progress was last seen.
    mark: u64,
    at: tokio::time::Instant,
}

impl DriverHealth {
    /// A driver that has not run yet counts as having made progress now,
    /// so a process that just started is live.
    pub fn new() -> Self {
        Self::with_limits(STALL_CUTOFF, STALLED_AFTER)
    }

    /// [`new`](Self::new) with the limits the operator set: a cycle with
    /// no progress for `stall_cutoff` is cut off, and `/livez` fails after
    /// `stalled_after` without progress.
    pub fn with_limits(stall_cutoff: Duration, stalled_after: Duration) -> Self {
        DriverHealth {
            stall_cutoff,
            stalled_after,
            pulse: Arc::new(LeaderPulse::default()),
            leg: Mutex::new("start"),
            legs_entered: AtomicU64::new(0),
            progress: Mutex::new(Progress {
                mark: 0,
                at: tokio::time::Instant::now(),
            }),
        }
    }

    fn enter(&self, leg: &'static str) {
        *self.leg.lock().unwrap_or_else(|p| p.into_inner()) = leg;
        self.legs_entered.fetch_add(1, Ordering::Relaxed);
    }

    fn leg(&self) -> &'static str {
        *self.leg.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// How long a cycle may go without progress before it is cut off.
    pub fn stall_cutoff(&self) -> Duration {
        self.stall_cutoff
    }

    /// How long the driver may go without progress before `/livez` fails.
    pub fn stalled_after(&self) -> Duration {
        self.stalled_after
    }

    /// What the scheduler records each time it asks for its lease.
    pub fn pulse(&self) -> Arc<LeaderPulse> {
        self.pulse.clone()
    }

    /// Notes any progress since the last look, and returns how long ago
    /// progress was last seen.
    fn observe(&self) -> Duration {
        let mark = self.pulse.asks() + self.legs_entered.load(Ordering::Relaxed);
        let now = tokio::time::Instant::now();
        let mut p = self.progress.lock().unwrap_or_else(|p| p.into_inner());
        if mark != p.mark {
            p.mark = mark;
            p.at = now;
        }
        now.saturating_duration_since(p.at)
    }

    /// Why the driver counts as stalled, or `None`: it has made no progress
    /// for more than `stalled_after`. Every replica's driver runs a cycle
    /// each interval, leader or not, so this holds for a replica waiting to
    /// take the lease over too: a stuck one could not take over.
    pub fn stalled(&self, stalled_after: Duration) -> Option<String> {
        let idle = self.observe();
        if idle <= stalled_after {
            return None;
        }
        let role = if self.pulse.held() {
            "holds the scheduler lease"
        } else {
            "does not hold the scheduler lease"
        };
        Some(format!(
            "the scheduler driver {role} and has made no progress for {}s (last in: {})",
            idle.as_secs(),
            self.leg()
        ))
    }
}

impl Default for DriverHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// Run `cycle` every `interval` (git-bug afa9f07). A cycle that makes no
/// progress for `stall_cutoff` is cut off with a WARN naming the part it
/// was in, and the next cycle starts; a cycle that keeps making progress
/// runs as long as it takes. Never returns.
pub async fn drive<F>(
    driver: &DriverHealth,
    interval: Duration,
    stall_cutoff: Duration,
    mut cycle: impl FnMut() -> F,
) where
    F: std::future::Future<Output = Result<(), SchedulerError>>,
{
    loop {
        driver.enter("start");
        let run = cycle();
        tokio::pin!(run);
        let mut check = tokio::time::interval(PROGRESS_CHECK);
        loop {
            tokio::select! {
                result = &mut run => {
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "converged driver tick failed");
                    }
                    break;
                }
                _ = check.tick() => {
                    if driver.observe() > stall_cutoff {
                        tracing::warn!(
                            part = driver.leg(),
                            stalled_secs = stall_cutoff.as_secs(),
                            "a converged driver cycle made no progress within its limit; it was \
                             cut off and the next cycle starts"
                        );
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}

static DRIVER_STOPPED: AtomicBool = AtomicBool::new(false);
static DRIVER_STOPPED_NOTIFY: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Mark the driver stopped and wake every [`driver_stopped`] waiter.
fn signal_driver_stopped() {
    DRIVER_STOPPED.store(true, Ordering::SeqCst);
    DRIVER_STOPPED_NOTIFY.notify_waiters();
}

/// Resolves once the converged driver has stopped (git-bug afa9f07); never
/// in a process whose driver keeps running. `main` treats it as a shutdown
/// signal.
pub async fn driver_stopped() {
    loop {
        let notified = DRIVER_STOPPED_NOTIFY.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if DRIVER_STOPPED.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

/// Whether the converged driver stopped: `main` exits non-zero after its
/// graceful shutdown when it did.
pub fn driver_has_stopped() -> bool {
    DRIVER_STOPPED.load(Ordering::SeqCst)
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

    /// git-bug afa9f07: a cycle that makes no progress is cut off after
    /// the stall cutoff and the next cycle runs; a cycle that keeps making
    /// progress is never cut, however long it takes.
    #[tokio::test(start_paused = true)]
    async fn a_stuck_cycle_is_cut_off_and_a_slow_one_is_not() {
        let driver = DriverHealth::new();
        let runs = AtomicUsize::new(0);
        let finished = AtomicUsize::new(0);
        let looping = drive(&driver, Duration::from_millis(500), STALL_CUTOFF, || {
            let n = runs.fetch_add(1, Ordering::SeqCst);
            let (driver, finished) = (&driver, &finished);
            async move {
                match n {
                    // Stuck: never returns, makes no progress.
                    0 => {
                        driver.enter("forge status drain");
                        std::future::pending::<()>().await;
                    }
                    // Slow but working: 5 minutes, asking for the lease
                    // every 10 seconds as a leader leg does.
                    1 => {
                        for _ in 0..30 {
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            driver.pulse().record(true);
                        }
                    }
                    _ => return Ok(()),
                }
                // Reached only by the slow cycle: the stuck one never returns.
                finished.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        tokio::select! {
            () = looping => unreachable!("drive never returns"),
            () = async {
                while runs.load(Ordering::SeqCst) < 3 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            } => {}
        }
        assert_eq!(
            finished.load(Ordering::SeqCst),
            1,
            "the slow cycle finished: it was not cut"
        );
    }

    /// `/livez` fails for any replica whose driver stopped making progress,
    /// leader or not.
    #[tokio::test(start_paused = true)]
    async fn stalled_for_any_replica_without_progress() {
        let driver = DriverHealth::new();
        tokio::time::advance(Duration::from_secs(300)).await;
        let why = driver
            .stalled(STALLED_AFTER)
            .expect("a stuck non-leader is stalled");
        assert!(why.contains("does not hold the scheduler lease"), "{why}");

        driver.pulse().record(true);
        assert_eq!(
            driver.stalled(STALLED_AFTER),
            None,
            "it just asked for the lease"
        );
        driver.enter("scheduler tick");
        assert_eq!(driver.stalled(STALLED_AFTER), None, "it entered a part");
        tokio::time::advance(STALLED_AFTER).await;
        assert_eq!(driver.stalled(STALLED_AFTER), None, "at the bound");
        tokio::time::advance(Duration::from_secs(1)).await;
        let why = driver.stalled(STALLED_AFTER).expect("stalled");
        assert!(
            why.contains("91s") && why.contains("scheduler tick"),
            "{why}"
        );

        driver.pulse().record(true);
        assert_eq!(driver.stalled(STALLED_AFTER), None, "progress again");
        tokio::time::advance(Duration::from_secs(120)).await;
        // Asking for the lease and not getting it is progress too.
        driver.pulse().record(false);
        assert_eq!(driver.stalled(STALLED_AFTER), None, "asked, not granted");
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

    /// The stop signal wakes a waiter that started before it and resolves
    /// at once for one that starts after.
    #[tokio::test]
    async fn driver_stopped_wakes_early_and_late_waiters() {
        let early = tokio::spawn(driver_stopped());
        tokio::task::yield_now().await;
        assert!(!driver_has_stopped());
        signal_driver_stopped();
        tokio::time::timeout(Duration::from_secs(1), early)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), driver_stopped())
            .await
            .unwrap();
        assert!(driver_has_stopped());
    }
}
