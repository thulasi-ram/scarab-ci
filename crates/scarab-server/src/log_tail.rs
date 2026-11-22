//! Log **source** wiring (ADR-0013): read a unit's container log into the
//! [`LogService`] store, fenced, redacted with the unit's value ledger.
//!
//! The source is the executor's followed log read ([`Executor::log_stream`]
//! and its sidecar and service forms), an agentless control-plane pull,
//! separate from the acked results channel (ADR-0042): logs are best-effort,
//! so a failed or dropped tail never fails the run.
//!
//! One read of a stream (git-bug 511a769, 2d78464, 93b7733, 772c34f,
//! 85b29e0):
//! 1. claims the stream's tail lease; every row it appends is fenced on that
//!    term ([`scarab_engine::Db::append_log_row`]);
//! 2. loads the unit's value ledger, all of it or nothing. A unit with no
//!    ledger, a purged one, or a source that answers
//!    [`ExecError::NotLedgerComplete`] gets one `not_stored` marker and the
//!    stream is closed: no source byte is read;
//! 3. reads the source from its first byte, redacts it as one stream, and
//!    compares it with the stream's **current part** (the output rows after
//!    the newest `source_restarted` row) by the digests of those rows. A
//!    difference stores a `source_restarted` row and the whole source again;
//!    what follows the part is appended;
//! 4. on a clean end, asks the backend whether the container ended
//!    ([`Executor::log_source_state`]): only a confirmed end completes the
//!    stream; a dropped connection is a broken read, retried.
//!
//! Markers are rows with a `kind`, never bodies, so nothing a step prints
//! changes how its log is stored.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::FutureExt;
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Db, ExecError, Executor, LogAppendError, LogChunkMeta,
    LogChunks, LogFrom, LogLease, LogRowKind, LogSourceRef, LogSourceState, LogStreamId,
    LogStreamState, LogWriter, NotStoredReason, RunId, StepId, StepRun, StepStatus, Timestamp,
};

pub use crate::logs::tail_lease_resource;
use crate::logs::{LedgerError, LogError, LogService, ValueSet};

/// Errors from [`pump_log_stream`].
#[derive(Debug, thiserror::Error)]
pub enum LogTailError {
    #[error("executor error: {0}")]
    Exec(#[from] scarab_engine::ExecError),
    #[error("log pipeline error: {0}")]
    Log(#[from] LogError),
    #[error("log append refused: {0}")]
    Append(#[from] LogAppendError),
    #[error("{0}")]
    Ledger(#[from] LedgerError),
}

/// Bytes a log tail gathers before it stores them as one row.
pub const TAIL_CHUNK_BYTES: usize = 256 * 1024;

/// The longest a log tail holds bytes it has read before it stores them, so
/// a live viewer sees a quiet step's output within this time.
pub const TAIL_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// The tail-lease TTL (ADR-0051): long enough to survive a slow tick, short
/// enough that a crashed replica's streams are read again within a minute.
const TAIL_LEASE_TTL_MS: i64 = 45_000;

/// How much longer than the retry backoff a tail lease lasts once its read
/// has ended (see [`give_up_lease`]).
const TAIL_DONE_MARGIN: Duration = Duration::from_secs(2);

/// How long a step Pod may sit in a benign pre-start state before the quiet
/// retry loop is reported (c653742).
const NOT_READY_GRACE: Duration = Duration::from_secs(180);

/// Once past [`NOT_READY_GRACE`], restate a stuck tail at most this often.
const STUCK_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// How often the settle pass and the pending-final-read query run.
const SWEEP_EVERY: Duration = Duration::from_secs(30);

/// Units per leg of one pending-final-read pass.
const PENDING_PAGE: u32 = 64;

/// Timing of the read state machine. [`TailTiming::default`] is production;
/// tests shorten it.
#[derive(Debug, Clone, Copy)]
pub struct TailTiming {
    /// Wait before re-reading a stream whose live read failed or ended
    /// unconfirmed.
    pub retry_backoff: Duration,
    /// The first and longest wait between final reads (doubling).
    pub final_backoff: Duration,
    pub final_backoff_max: Duration,
    /// After a clean end, how long the state is polled for the container's
    /// end, and how often.
    pub end_wait: Duration,
    pub end_poll: Duration,
    /// How much later than the clean end a container may have finished and
    /// still confirm it (backend clock against this server's).
    pub end_skew_ms: i64,
    /// A read of an ended unit with no byte for this long asks the state.
    pub idle: Duration,
    /// How long final reads may fail before the stream is closed.
    pub final_budget_ms: i64,
}

impl Default for TailTiming {
    fn default() -> Self {
        Self {
            retry_backoff: Duration::from_secs(3),
            final_backoff: Duration::from_secs(3),
            final_backoff_max: Duration::from_secs(60),
            end_wait: Duration::from_secs(10),
            end_poll: Duration::from_millis(500),
            end_skew_ms: 2_000,
            idle: Duration::from_secs(10),
            final_budget_ms: 30 * 60 * 1000,
        }
    }
}

/// The unit whose log a tail reads.
#[derive(Clone)]
enum TailSource {
    Step(StepRun),
    Service(scarab_engine::ports::ExecHandle),
    /// The `index`-th sidecar service of `step`: container `service-{index}`
    /// in the step's own Pod.
    Sidecar {
        step: StepRun,
        index: usize,
    },
}

impl TailSource {
    fn source_ref(&self) -> LogSourceRef {
        match self {
            TailSource::Step(step) => LogSourceRef::Step(step.clone()),
            TailSource::Service(h) => LogSourceRef::Service(h.clone()),
            TailSource::Sidecar { step, index } => LogSourceRef::Sidecar {
                step: step.clone(),
                container: format!("service-{index}"),
            },
        }
    }

    async fn open(&self, executor: &dyn Executor) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        match self {
            TailSource::Step(step) => executor.log_stream(step, LogFrom::Start).await,
            TailSource::Service(h) => executor.service_log_stream(h, LogFrom::Start).await,
            TailSource::Sidecar { step, index } => {
                executor
                    .sidecar_log_stream(step, &format!("service-{index}"), LogFrom::Start)
                    .await
            }
        }
    }
}

/// A step run with one attempt, rebuilt from a unit id: what the executor
/// needs to address the unit's Pod.
fn step_run_of(unit: &LogStreamId) -> StepRun {
    StepRun {
        key: None,
        run: unit.run.clone(),
        step: unit.step.clone(),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: unit.attempt.clone(),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Set when a stream's unit leaves `Running` while a live read of it is in
/// flight: the read then applies the idle rule.
#[derive(Default)]
struct EndSignal(AtomicBool);

/// A fence tailed while its unit ran and not terminal yet: how to read it
/// once more after the unit ends.
#[derive(Clone)]
struct OpenTail {
    source: TailSource,
    signal: Arc<EndSignal>,
}

#[derive(Default)]
struct TailState {
    /// Fences with a read in flight.
    active: HashSet<LogStreamId>,
    /// Fences found terminal (`complete` or `closed`), or with no log source:
    /// never read again by this process.
    done: HashSet<LogStreamId>,
    retry_at: HashMap<LogStreamId, Instant>,
    /// The wait before the next final read of a fence (doubling).
    final_backoff: HashMap<LogStreamId, Duration>,
    open: HashMap<LogStreamId, OpenTail>,
    /// Fences ensured since the last [`LogTailer::end_pass`].
    seen: HashSet<LogStreamId>,
    not_ready: HashMap<LogStreamId, NotReady>,
}

/// A tail parked in a benign pre-start state: since when, and when it was
/// last reported.
#[derive(Clone, Copy)]
struct NotReady {
    since: Instant,
    last_warn: Option<Instant>,
}

/// Spawns and tracks per-stream log reads. The converged driver calls
/// [`ensure`](LogTailer::ensure) (and its sidecar and service forms) for
/// every running unit each tick, then [`end_pass`](LogTailer::end_pass).
pub struct LogTailer {
    executor: Arc<dyn Executor>,
    logs: Arc<LogService>,
    db: Arc<dyn Db>,
    /// This process's lease owner id.
    owner: String,
    timing: TailTiming,
    state: Arc<Mutex<TailState>>,
    last_sweep: Mutex<Option<Instant>>,
    /// Where the next pending-final-read pass resumes.
    pending_cursor: Mutex<scarab_engine::PendingReadCursor>,
}

impl LogTailer {
    pub fn new(executor: Arc<dyn Executor>, logs: Arc<LogService>) -> Self {
        Self {
            executor,
            db: logs.db().clone(),
            logs,
            owner: format!("tail-{}", uuid::Uuid::new_v4()),
            timing: TailTiming::default(),
            state: Arc::new(Mutex::new(TailState::default())),
            last_sweep: Mutex::new(None),
            pending_cursor: Mutex::new(Default::default()),
        }
    }

    /// Wait `backoff` instead of the default before re-reading a stream
    /// whose last read failed (and as the first final-read backoff).
    pub fn with_retry_backoff(mut self, backoff: Duration) -> Self {
        self.timing.retry_backoff = backoff;
        self.timing.final_backoff = backoff;
        self.timing.final_backoff_max = self.timing.final_backoff_max.max(backoff);
        self
    }

    /// Use `timing` for the read state machine (tests).
    pub fn with_timing(mut self, timing: TailTiming) -> Self {
        self.timing = timing;
        self
    }

    /// Take tail leases (ADR-0051) in `db` as `owner`: every read holds the
    /// stream's lease and every row it appends is fenced on its term.
    pub fn with_lease(mut self, db: Arc<dyn Db>, owner: impl Into<String>) -> Self {
        self.db = db;
        self.owner = owner.into();
        self
    }

    /// Ensure a read runs for `step`'s current attempt. Idempotent per
    /// stream; a step with no attempt yet is a no-op.
    pub fn ensure(&self, step: &StepRun) {
        let Some(attempt) = step.current_attempt() else {
            return;
        };
        let fence = LogStreamId::new(&step.run, &step.step, &attempt.id);
        self.spawn_tail(fence, TailSource::Step(step.clone()), false);
    }

    /// Ensure a read runs for a **shared service** instance (ADR-0058).
    pub fn ensure_service(&self, run: &RunId, take: i64, name: &str, handle: &str) {
        let (step, attempt) = crate::logs::service_stream_key(name, take);
        let fence = LogStreamId::new(run, &step, &attempt);
        self.spawn_tail(
            fence,
            TailSource::Service(scarab_engine::ports::ExecHandle(handle.to_string())),
            false,
        );
    }

    /// Ensure a read runs for the `index`-th **sidecar service** of `step`.
    pub fn ensure_sidecar(&self, step: &StepRun, index: usize) {
        let Some(attempt) = step.current_attempt() else {
            return;
        };
        let syn = crate::logs::sidecar_stream_key(&step.step, index);
        let fence = LogStreamId::new(&step.run, &syn, &attempt.id);
        self.spawn_tail(
            fence,
            TailSource::Sidecar {
                step: step.clone(),
                index,
            },
            false,
        );
    }

    /// End one driver pass of `ensure*` calls. A stream tailed here and not
    /// ensured in this pass belongs to a unit that left `Running`: an open
    /// live read of it is told so (the idle rule), and a final read runs
    /// once it is not in flight, with a doubling backoff, until the stream
    /// is terminal. `complete` is false when the pass did not ensure every
    /// unit (a store error); such a pass only resets the bookkeeping.
    pub fn end_pass(&self, complete: bool) {
        let ended: Vec<(LogStreamId, OpenTail)> = {
            let mut st = self.state.lock().unwrap();
            let seen = std::mem::take(&mut st.seen);
            if !complete {
                return;
            }
            st.open
                .iter()
                .filter(|(fence, _)| !seen.contains(*fence))
                .map(|(f, t)| (f.clone(), t.clone()))
                .collect()
        };
        for (fence, tail) in ended {
            tail.signal.0.store(true, Ordering::SeqCst);
            self.spawn_tail(fence, tail.source, true);
        }
    }

    /// The periodic part of a driver pass, at most every 30 s: mark units
    /// whose attempt or service left `Running` settled, and read once more
    /// the units that left `Running` while no tail of this process saw them
    /// (the pending-final-read query). Both are database work and spawned
    /// reads; no forge call is made here. Revoking and purging ledgers is
    /// the ledger sweep's ([`spawn_ledger_sweeper`]), a task of its own.
    pub async fn sweep(&self) {
        {
            let mut last = self.last_sweep.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < SWEEP_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        let now = Timestamp(now_ms());
        if let Err(e) = self.db.settle_log_units(now).await {
            tracing::debug!(error = %e, "log ledger: marking settled units failed");
        }
        self.pending_pass().await;
    }

    /// Read the units the pending-final-read query names: one page of each
    /// of its legs, resuming where the last pass stopped (a leg that ran
    /// out starts again from its first key), so units that keep failing
    /// here cannot hide the ones behind them.
    pub async fn pending_pass(&self) {
        let cursor = self.pending_cursor.lock().unwrap().clone();
        let page = match self
            .db
            .pending_final_reads(Timestamp(now_ms()), &cursor, PENDING_PAGE)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %e, "log tail: the pending-final-read query failed");
                return;
            }
        };
        *self.pending_cursor.lock().unwrap() = page.next;
        for p in page.reads {
            let unit = p.unit.clone();
            let service = unit.step.0.starts_with("service:");
            if !p.has_ledger {
                // Its sidecars, from the step's spec: a unit with no ledger
                // recorded no stream count.
                let sidecars = if service {
                    0
                } else {
                    match self.db.step_spec(&unit.run, &unit.step).await {
                        Ok(spec) => spec.map_or(0, |s| s.services.len()),
                        Err(_) => continue,
                    }
                };
                self.spawn_no_ledger_close(unit, p.started, sidecars);
                continue;
            }
            if service {
                if let Some(h) = p.handle.clone() {
                    self.spawn_tail(
                        unit,
                        TailSource::Service(scarab_engine::ports::ExecHandle(h)),
                        true,
                    );
                }
                continue;
            }
            // Every stream of the unit, whatever the state of the others: a
            // read of a terminal stream ends at its row.
            let step = step_run_of(&unit);
            self.spawn_tail(unit.clone(), TailSource::Step(step.clone()), true);
            for i in 0..(p.stream_count.max(1) as usize - 1) {
                let syn = crate::logs::sidecar_stream_key(&unit.step, i);
                let fence = LogStreamId::new(&unit.run, &syn, &unit.attempt);
                self.spawn_tail(
                    fence,
                    TailSource::Sidecar {
                        step: step.clone(),
                        index: i,
                    },
                    true,
                );
            }
        }
    }

    /// Close the streams of a unit that has no ledger and left `Running`
    /// (an earlier version launched it, or it never launched): its
    /// `sidecars` sidecar streams, then its main stream, each with one
    /// `not_stored` marker if the unit started, else none, and `closed`. No
    /// source byte is read. Once all are closed the unit's
    /// `log_legacy_units` row, if any, is deleted.
    fn spawn_no_ledger_close(&self, unit: LogStreamId, started: bool, sidecars: usize) {
        if !self.state.lock().unwrap().active.insert(unit.clone()) {
            return;
        }
        let ctx = self.ctx();
        tokio::spawn(async move {
            let mut fences: Vec<LogStreamId> = (0..sidecars)
                .map(|i| {
                    LogStreamId::new(
                        &unit.run,
                        &crate::logs::sidecar_stream_key(&unit.step, i),
                        &unit.attempt,
                    )
                })
                .collect();
            fences.push(unit.clone());
            let mut closed = true;
            for fence in &fences {
                if !close_no_ledger(&ctx, fence, started).await {
                    closed = false;
                    break;
                }
            }
            if closed {
                if let Err(e) = ctx.db.forget_log_legacy_unit(&unit).await {
                    tracing::debug!(error = %e, "log tail: forgetting a closed earlier-version unit failed; closed again later");
                }
            }
            ctx.state.lock().unwrap().active.remove(&unit);
        });
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            executor: self.executor.clone(),
            logs: self.logs.clone(),
            db: self.db.clone(),
            owner: self.owner.clone(),
            timing: self.timing,
            state: self.state.clone(),
        }
    }

    /// Dedup, back off, and spawn one read of `fence`.
    fn spawn_tail(&self, fence: LogStreamId, source: TailSource, final_read: bool) {
        let signal = {
            let mut st = self.state.lock().unwrap();
            if !final_read {
                st.seen.insert(fence.clone());
            }
            if st.done.contains(&fence) {
                return;
            }
            let signal = if final_read {
                st.open
                    .get(&fence)
                    .map(|t| t.signal.clone())
                    .unwrap_or_else(|| Arc::new(EndSignal(AtomicBool::new(true))))
            } else {
                st.open
                    .entry(fence.clone())
                    .or_insert_with(|| OpenTail {
                        source: source.clone(),
                        signal: Arc::new(EndSignal::default()),
                    })
                    .signal
                    .clone()
            };
            if let Some(at) = st.retry_at.get(&fence) {
                if Instant::now() < *at {
                    return;
                }
            }
            if !st.active.insert(fence.clone()) {
                return;
            }
            signal
        };
        let ctx = self.ctx();
        tokio::spawn(async move {
            let outcome =
                std::panic::AssertUnwindSafe(run_tail(&ctx, &fence, &source, final_read, &signal))
                    .catch_unwind()
                    .await
                    .unwrap_or(Outcome::Retry);
            let mut st = ctx.state.lock().unwrap();
            st.active.remove(&fence);
            match outcome {
                Outcome::Terminal | Outcome::NoSource => {
                    st.done.insert(fence.clone());
                    st.open.remove(&fence);
                    st.retry_at.remove(&fence);
                    st.final_backoff.remove(&fence);
                    st.not_ready.remove(&fence);
                }
                Outcome::Retry if final_read => {
                    let wait = st
                        .final_backoff
                        .get(&fence)
                        .copied()
                        .unwrap_or(ctx.timing.final_backoff);
                    st.retry_at.insert(fence.clone(), Instant::now() + wait);
                    st.final_backoff
                        .insert(fence.clone(), (wait * 2).min(ctx.timing.final_backoff_max));
                }
                Outcome::Retry => {
                    st.retry_at
                        .insert(fence.clone(), Instant::now() + ctx.timing.retry_backoff);
                }
                // The unit ended during a live read: the final read runs
                // at the next pass, without a wait.
                Outcome::Again => {
                    st.retry_at.remove(&fence);
                }
                Outcome::Stop => {}
            }
        });
    }
}

/// What a read task needs, cloned out of the tailer.
#[derive(Clone)]
struct Ctx {
    executor: Arc<dyn Executor>,
    logs: Arc<LogService>,
    db: Arc<dyn Db>,
    owner: String,
    timing: TailTiming,
    state: Arc<Mutex<TailState>>,
}

/// How one read task ended.
enum Outcome {
    /// The stream is terminal.
    Terminal,
    /// The backend has no log source (the local executor): nothing to read.
    NoSource,
    /// Read again after the backoff.
    Retry,
    /// Read again at the next pass, without the backoff.
    Again,
    /// Another replica holds or took the lease: nothing to do here.
    Stop,
}

/// Claim `resource`, a stream's tail lease, for this process. `Err`:
/// another replica holds it.
async fn claim(ctx: &Ctx, resource: &str) -> Result<LogWriter, ()> {
    match ctx.db.lease(resource, &ctx.owner, TAIL_LEASE_TTL_MS).await {
        Ok(l) => l
            .held_by(resource, &ctx.owner)
            .map(|held| LogWriter::holding(LogLease::Tail, &held))
            .ok_or(()),
        Err(_) => Err(()),
    }
}

/// Stop holding a lease a read held: cut it to the retry backoff plus
/// [`TAIL_DONE_MARGIN`], so a live lease means a read is in progress or
/// ended less than that ago (`log_stream` reads it).
async fn give_up_lease(ctx: &Ctx, resource: &str, backoff: Duration) {
    let ttl_ms = (backoff + TAIL_DONE_MARGIN).as_millis() as i64;
    if let Err(e) = ctx.db.lease(resource, &ctx.owner, ttl_ms).await {
        tracing::debug!(resource, error = %e, "log tail: could not shorten the tail lease; it expires");
    }
}

/// Close `fence`, a stream of a unit with no ledger, under its tail lease:
/// one `not_stored` (`no_ledger`) marker if the unit `started`, else none,
/// and `closed`. No source byte is read. Whether it is closed now.
async fn close_no_ledger(ctx: &Ctx, fence: &LogStreamId, started: bool) -> bool {
    let resource = tail_lease_resource(&fence.run, &fence.step, &fence.attempt);
    let Ok(writer) = claim(ctx, &resource).await else {
        return false;
    };
    let now = Timestamp(now_ms());
    let r = if started {
        ctx.logs
            .append_marker(
                fence,
                LogRowKind::NotStored,
                Some(NotStoredReason::NoLedger),
                &writer,
                Some(LogStreamState::Closed),
                now,
            )
            .await
            .map(|_| ())
    } else {
        ctx.db
            .set_log_stream_state(fence, &writer, LogStreamState::Closed, now)
            .await
    };
    let closed = match r {
        Ok(()) | Err(LogAppendError::Closed) => {
            ctx.state.lock().unwrap().done.insert(fence.clone());
            true
        }
        Err(e) => {
            tracing::debug!(error = %e, "log tail: closing a stream with no ledger failed");
            false
        }
    };
    give_up_lease(ctx, &resource, ctx.timing.retry_backoff).await;
    closed
}

/// Renew `resource` every third of its TTL while a read runs; set `lost`
/// when another owner holds it, or renewals fail for longer than half the
/// TTL. The read checks `lost` and ends at once.
fn spawn_renewer(ctx: &Ctx, resource: &str, lost: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    let db = ctx.db.clone();
    let owner = ctx.owner.clone();
    let resource = resource.to_string();
    tokio::spawn(async move {
        let mut failing_since: Option<Instant> = None;
        loop {
            tokio::time::sleep(Duration::from_millis((TAIL_LEASE_TTL_MS / 3) as u64)).await;
            match db.lease(&resource, &owner, TAIL_LEASE_TTL_MS).await {
                Ok(l) if l.owner == owner => failing_since = None,
                Ok(_) => {
                    lost.store(true, Ordering::SeqCst);
                    return;
                }
                Err(_) => {
                    let since = *failing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > Duration::from_millis((TAIL_LEASE_TTL_MS / 2) as u64) {
                        lost.store(true, Ordering::SeqCst);
                        return;
                    }
                }
            }
        }
    })
}

/// One read task: claim, read, release.
async fn run_tail(
    ctx: &Ctx,
    fence: &LogStreamId,
    source: &TailSource,
    final_read: bool,
    signal: &EndSignal,
) -> Outcome {
    let resource = tail_lease_resource(&fence.run, &fence.step, &fence.attempt);
    let Ok(writer) = claim(ctx, &resource).await else {
        return Outcome::Retry;
    };
    let lost = Arc::new(AtomicBool::new(false));
    let renewer = spawn_renewer(ctx, &resource, lost.clone());
    let outcome = read_stream(ctx, fence, source, final_read, signal, &writer, &lost).await;
    renewer.abort();
    give_up_lease(ctx, &resource, ctx.timing.retry_backoff).await;
    outcome
}

/// Whether a failure is local to this replica (I6): it never counts toward
/// the final-read budget and never closes a stream.
enum Failure {
    /// The ledger could not be loaded here, or the store failed.
    Local(String),
    /// The source could not be read to a confirmed end.
    Source(String),
}

/// The reads of one task over `fence`, until it is terminal or a read
/// fails. A difference with the stored part or a ledger generation that
/// moved re-reads at once, a bounded number of times.
async fn read_stream(
    ctx: &Ctx,
    fence: &LogStreamId,
    source: &TailSource,
    final_read: bool,
    signal: &EndSignal,
    writer: &LogWriter,
    lost: &AtomicBool,
) -> Outcome {
    let unit = fence.unit();
    let mut failure = None;
    for _ in 0..6 {
        let row = match ctx.db.log_stream_row(fence).await {
            Ok(r) => r,
            Err(e) => return report(ctx, fence, Failure::Local(e.to_string())),
        };
        if row.as_ref().is_some_and(|r| r.state.is_terminal()) {
            return Outcome::Terminal;
        }
        let values = match ctx.logs.load_values(&unit).await {
            Ok(v) => Some(v),
            // No ledger row: a 0.7 launch writes it before the Pod exists,
            // so either an earlier version launched the unit or it has not
            // launched yet. The source tells which (its label).
            Err(LedgerError::NotLedgerComplete(NotStoredReason::NoLedger)) => None,
            Err(LedgerError::NotLedgerComplete(reason)) => {
                return close_not_stored(ctx, fence, writer, reason).await;
            }
            Err(LedgerError::Local(e)) => return report(ctx, fence, Failure::Local(e)),
        };
        let before = ctx
            .executor
            .log_source_state(&source.source_ref())
            .await
            .ok();
        let chunks = match source.open(ctx.executor.as_ref()).await {
            Ok(Some(c)) => c,
            Ok(None) => return Outcome::NoSource,
            Err(ExecError::NotLedgerComplete(_)) => {
                return close_not_stored(ctx, fence, writer, NotStoredReason::NoLedger).await;
            }
            Err(e) => {
                if final_read && matches!(before, Some(LogSourceState::Gone)) {
                    return gone_unconfirmed(ctx, fence, &unit, writer, row.is_some()).await;
                }
                failure = Some(Failure::Source(e.to_string()));
                break;
            }
        };
        let Some(values) = values else {
            // A source that opened (a labelled unit) with no ledger row:
            // not read.
            failure = Some(Failure::Local(
                "the unit's log opened but it has no ledger row".into(),
            ));
            break;
        };
        let _ = ctx
            .db
            .mark_log_unit_started(&unit, Timestamp(now_ms()))
            .await;
        let part_start = row.as_ref().map_or(0, |r| r.part_start_seq);
        let part = match ctx
            .logs
            .chunks_from(&fence.run, &fence.step, &fence.attempt, part_start)
            .await
        {
            Ok(rows) => rows.into_iter().filter(|r| r.is_output()).collect(),
            Err(e) => return report(ctx, fence, Failure::Local(e.to_string())),
        };
        let read = Read {
            ctx,
            fence,
            source,
            writer,
            lost,
            signal,
            final_read,
            before,
        };
        match read.pump(chunks, values, part, part_start > 0).await {
            PumpEnd::Complete => {
                ctx.logs.purge_if_done(&unit, None).await;
                return Outcome::Terminal;
            }
            PumpEnd::Restarting => return Outcome::Again,
            PumpEnd::Mismatch => {
                tracing::warn!(
                    run = %fence.run.0, step = %fence.step.0, attempt = %fence.attempt.0,
                    "log tail: the log does not begin with the part already stored \
                     (rotated log file, restarted container, or new secret values); \
                     storing it again after a marker"
                );
                if let Err(e) = ctx
                    .logs
                    .append_marker(
                        fence,
                        LogRowKind::SourceRestarted,
                        None,
                        writer,
                        None,
                        Timestamp(now_ms()),
                    )
                    .await
                {
                    return refused(ctx, fence, writer, e).await;
                }
            }
            PumpEnd::ValuesChanged => {
                let _ = ctx.logs.reload_values(&unit).await;
            }
            PumpEnd::IdleEnded if final_read => {}
            PumpEnd::IdleEnded => return Outcome::Again,
            PumpEnd::Refused(e) => return refused(ctx, fence, writer, e).await,
            PumpEnd::Broken(f) => {
                failure = Some(f);
                break;
            }
        }
    }
    let failure = failure.unwrap_or_else(|| Failure::Source("the log kept changing".into()));
    if final_read {
        if let Failure::Source(_) = &failure {
            if let Some(o) = check_budget(ctx, fence, &unit, writer).await {
                return o;
            }
        }
    }
    report(ctx, fence, failure)
}

/// Log a failed read and retry it after the backoff.
fn report(ctx: &Ctx, fence: &LogStreamId, failure: Failure) -> Outcome {
    let msg = match &failure {
        Failure::Local(m) | Failure::Source(m) => m.clone(),
    };
    report_live_error(&ctx.state, fence, &msg);
    Outcome::Retry
}

/// A refused append: the stream is terminal (`Closed`), the lease moved
/// (`NotWriter`), or the unit's ledger is gone (a marker closes it).
async fn refused(ctx: &Ctx, fence: &LogStreamId, writer: &LogWriter, e: LogAppendError) -> Outcome {
    match e {
        LogAppendError::Closed => Outcome::Terminal,
        LogAppendError::NotWriter => Outcome::Stop,
        LogAppendError::NoLedger => {
            close_not_stored(ctx, fence, writer, NotStoredReason::NoLedger).await
        }
        LogAppendError::LedgerPurged => {
            close_not_stored(ctx, fence, writer, NotStoredReason::LedgerPurged).await
        }
        LogAppendError::ValuesChanged { .. } => Outcome::Again,
        LogAppendError::Db(e) => report(ctx, fence, Failure::Local(e.to_string())),
    }
}

/// The unit is not ledger-complete: one `not_stored` marker, `closed`, no
/// source byte read.
async fn close_not_stored(
    ctx: &Ctx,
    fence: &LogStreamId,
    writer: &LogWriter,
    reason: NotStoredReason,
) -> Outcome {
    tracing::warn!(
        run = %fence.run.0, step = %fence.step.0, attempt = %fence.attempt.0,
        reason = reason.as_str(),
        "log tail: the unit is not ledger-complete; the rest of its log is not stored"
    );
    match ctx
        .logs
        .append_marker(
            fence,
            LogRowKind::NotStored,
            Some(reason),
            writer,
            Some(LogStreamState::Closed),
            Timestamp(now_ms()),
        )
        .await
    {
        Ok(_) | Err(LogAppendError::Closed) => Outcome::Terminal,
        Err(LogAppendError::NotWriter) => Outcome::Stop,
        Err(e) => report(ctx, fence, Failure::Local(e.to_string())),
    }
}

/// Whether the unit started: its ledger says so, or the stream has rows.
async fn unit_started(ctx: &Ctx, unit: &LogStreamId, has_rows: bool) -> bool {
    has_rows
        || ctx
            .db
            .log_unit_ledger(unit)
            .await
            .ok()
            .flatten()
            .is_some_and(|l| l.unit.started_at.is_some())
}

/// A final read found the unit gone before its end was confirmed: an
/// `incomplete` marker if it started, and `closed`.
async fn gone_unconfirmed(
    ctx: &Ctx,
    fence: &LogStreamId,
    unit: &LogStreamId,
    writer: &LogWriter,
    has_rows: bool,
) -> Outcome {
    close_incomplete(ctx, fence, writer, unit_started(ctx, unit, has_rows).await).await
}

async fn close_incomplete(
    ctx: &Ctx,
    fence: &LogStreamId,
    writer: &LogWriter,
    started: bool,
) -> Outcome {
    let now = Timestamp(now_ms());
    let r = if started {
        ctx.logs
            .append_marker(
                fence,
                LogRowKind::Incomplete,
                None,
                writer,
                Some(LogStreamState::Closed),
                now,
            )
            .await
            .map(|_| ())
    } else {
        ctx.db
            .set_log_stream_state(fence, writer, LogStreamState::Closed, now)
            .await
    };
    match r {
        Ok(()) | Err(LogAppendError::Closed) => {
            ctx.logs.purge_if_done(&fence.unit(), None).await;
            Outcome::Terminal
        }
        Err(LogAppendError::NotWriter) => Outcome::Stop,
        Err(e) => report(ctx, fence, Failure::Local(e.to_string())),
    }
}

/// After a final read that could not proceed: start the durable budget if
/// it has not started, and close the stream once it is spent (I9).
async fn check_budget(
    ctx: &Ctx,
    fence: &LogStreamId,
    unit: &LogStreamId,
    writer: &LogWriter,
) -> Option<Outcome> {
    let now = now_ms();
    let since = ctx
        .db
        .start_log_final_budget(fence, writer, Timestamp(now))
        .await
        .ok()?;
    if now - since < ctx.timing.final_budget_ms {
        return None;
    }
    tracing::warn!(
        run = %fence.run.0, step = %fence.step.0, attempt = %fence.attempt.0,
        "log tail: the end of an ended unit's log could not be read within the budget"
    );
    let has_rows = ctx
        .db
        .log_stream_row(fence)
        .await
        .ok()
        .flatten()
        .is_some_and(|r| r.next_seq > 0);
    let started = unit_started(ctx, unit, has_rows).await;
    Some(close_incomplete(ctx, fence, writer, started).await)
}

/// How one read of a source ended.
enum PumpEnd {
    /// The end was confirmed and the stream is `complete`.
    Complete,
    /// A restartable container ended and will start again; the stream stays
    /// open.
    Restarting,
    /// The source does not begin with the stored part. Nothing appended.
    Mismatch,
    /// The unit's ledger moved: reload and read again.
    ValuesChanged,
    /// The unit has ended and the read went quiet with the container ended
    /// or gone.
    IdleEnded,
    /// An append was refused.
    Refused(LogAppendError),
    /// The read failed or ended unconfirmed; held bytes dropped.
    Broken(Failure),
}

/// One read of a source.
struct Read<'a> {
    ctx: &'a Ctx,
    fence: &'a LogStreamId,
    source: &'a TailSource,
    writer: &'a LogWriter,
    lost: &'a AtomicBool,
    signal: &'a EndSignal,
    final_read: bool,
    /// The source state before the read opened.
    before: Option<LogSourceState>,
}

/// What [`Read::confirm_end`] found after a clean end.
enum EndKind {
    Confirmed,
    Restarting,
    Unconfirmed(String),
}

impl Read<'_> {
    async fn pump(
        &self,
        mut chunks: Box<dyn LogChunks>,
        values: Arc<ValueSet>,
        part: Vec<LogChunkMeta>,
        after_restart: bool,
    ) -> PumpEnd {
        let gen = values.gen();
        let mut redactor = Some(crate::logs::StreamRedactor::new(values, after_restart));
        let mut verify = Verifier::new(part);
        let mut batch = Batch::default();
        let mut last_byte = tokio::time::Instant::now();
        let tick = Duration::from_millis(200);
        loop {
            if self.lost.load(Ordering::SeqCst) {
                return PumpEnd::Refused(LogAppendError::NotWriter);
            }
            let next = {
                let fut = chunks.next_chunk();
                tokio::pin!(fut);
                loop {
                    tokio::select! {
                        next = &mut fut => break next,
                        () = tokio::time::sleep(tick) => {}
                    }
                    if self.lost.load(Ordering::SeqCst) {
                        return PumpEnd::Refused(LogAppendError::NotWriter);
                    }
                    if batch.due.is_some_and(|d| d <= tokio::time::Instant::now()) {
                        if let Err(e) = self.store(&mut batch, gen, None).await {
                            return e;
                        }
                    }
                    let armed = self.final_read || self.signal.0.load(Ordering::SeqCst);
                    if armed && last_byte.elapsed() >= self.ctx.timing.idle {
                        match self.state().await {
                            Some(LogSourceState::Ended { .. }) | Some(LogSourceState::Gone) => {
                                let _ = self.store(&mut batch, gen, None).await;
                                return PumpEnd::IdleEnded;
                            }
                            _ => last_byte = tokio::time::Instant::now(),
                        }
                    }
                }
            };
            let chunk = match next {
                Ok(Some(c)) if c.is_empty() => continue,
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    // What was gathered follows the stored part: store it,
                    // so the next read compares with it.
                    let _ = self.store(&mut batch, gen, None).await;
                    return PumpEnd::Broken(Failure::Source(e.to_string()));
                }
            };
            last_byte = tokio::time::Instant::now();
            let out = redactor.as_mut().expect("held until the end").push(&chunk);
            match verify.check(&out) {
                Err(()) => return PumpEnd::Mismatch,
                Ok(n) => batch.push(&out[n..]),
            }
            if batch.bytes.len() >= TAIL_CHUNK_BYTES {
                if let Err(e) = self.store(&mut batch, gen, None).await {
                    return e;
                }
            }
        }
        // A clean end: was it the container's?
        let t_e = now_ms();
        match self.confirm_end(t_e).await {
            EndKind::Unconfirmed(why) => {
                let _ = self.store(&mut batch, gen, None).await;
                PumpEnd::Broken(Failure::Source(why))
            }
            kind => {
                let held = redactor
                    .take()
                    .expect("held until the end")
                    .finish_confirmed();
                let n = match verify.check(&held) {
                    Err(()) => return PumpEnd::Mismatch,
                    Ok(n) => n,
                };
                batch.push(&held[n..]);
                if verify.ends_inside() && !self.ends_inside_matches(&verify).await {
                    return PumpEnd::Mismatch;
                }
                if matches!(kind, EndKind::Restarting) {
                    return match self.store(&mut batch, gen, None).await {
                        Ok(()) => PumpEnd::Restarting,
                        Err(e) => e,
                    };
                }
                if batch.bytes.is_empty() {
                    match self
                        .ctx
                        .db
                        .set_log_stream_state(
                            self.fence,
                            self.writer,
                            LogStreamState::Complete,
                            Timestamp(now_ms()),
                        )
                        .await
                    {
                        Ok(()) | Err(LogAppendError::Closed) => PumpEnd::Complete,
                        Err(e) => PumpEnd::Refused(e),
                    }
                } else {
                    match self
                        .store(&mut batch, gen, Some(LogStreamState::Complete))
                        .await
                    {
                        Ok(()) => PumpEnd::Complete,
                        Err(e) => e,
                    }
                }
            }
        }
    }

    async fn state(&self) -> Option<LogSourceState> {
        self.ctx
            .executor
            .log_source_state(&self.source.source_ref())
            .await
            .ok()
    }

    /// After a clean end at `t_e`: poll the source state for the
    /// container's end (2.4).
    async fn confirm_end(&self, t_e: i64) -> EndKind {
        let opened_id = match &self.before {
            Some(LogSourceState::Running { container_id }) => container_id.clone(),
            _ => None,
        };
        let opened_after_end = matches!(self.before, Some(LogSourceState::Ended { .. }));
        let deadline = tokio::time::Instant::now() + self.ctx.timing.end_wait;
        loop {
            match self.state().await {
                Some(LogSourceState::Ended { finished_at, .. }) => {
                    if opened_after_end || finished_at.0 <= t_e + self.ctx.timing.end_skew_ms {
                        return EndKind::Confirmed;
                    }
                    return EndKind::Unconfirmed("the read ended before the container did".into());
                }
                Some(LogSourceState::Gone) => return EndKind::Confirmed,
                Some(LogSourceState::Restarting) => return EndKind::Restarting,
                Some(LogSourceState::Running { container_id }) => {
                    if opened_id.is_some() && container_id.is_some() && container_id != opened_id {
                        return EndKind::Restarting;
                    }
                }
                None => return EndKind::Unconfirmed("the source state could not be read".into()),
            }
            if tokio::time::Instant::now() >= deadline {
                return EndKind::Unconfirmed("the log ended while the container still runs".into());
            }
            tokio::time::sleep(self.ctx.timing.end_poll).await;
        }
    }

    /// The source ended inside the last stored row: whether what it read of
    /// that row matches the row's body (one GET).
    async fn ends_inside_matches(&self, verify: &Verifier) -> bool {
        let Some((meta, partial)) = verify.inside() else {
            return true;
        };
        match self.ctx.logs.read_chunk(meta).await {
            Ok(body) => body.starts_with(partial),
            Err(_) => false,
        }
    }

    /// Store the gathered bytes as one output row (with `then`), if any.
    async fn store(
        &self,
        batch: &mut Batch,
        gen: i64,
        then: Option<LogStreamState>,
    ) -> Result<(), PumpEnd> {
        batch.due = None;
        if batch.bytes.is_empty() {
            return Ok(());
        }
        match self
            .ctx
            .logs
            .append_output(
                self.fence,
                &batch.bytes,
                gen,
                self.writer,
                then,
                Timestamp(now_ms()),
            )
            .await
        {
            Ok(_) => {
                batch.bytes.clear();
                Ok(())
            }
            Err(LogAppendError::ValuesChanged { .. }) => Err(PumpEnd::ValuesChanged),
            Err(LogAppendError::Db(e)) => Err(PumpEnd::Broken(Failure::Local(e.to_string()))),
            Err(e) => Err(PumpEnd::Refused(e)),
        }
    }
}

/// The bytes one read has gathered and not stored yet.
#[derive(Default)]
struct Batch {
    bytes: Vec<u8>,
    due: Option<tokio::time::Instant>,
}

impl Batch {
    fn push(&mut self, new: &[u8]) {
        if new.is_empty() {
            return;
        }
        if self.bytes.is_empty() {
            self.due = Some(tokio::time::Instant::now() + TAIL_FLUSH_INTERVAL);
        }
        self.bytes.extend_from_slice(new);
    }
}

/// Compares a read's redacted output with the stored part by the digests of
/// its rows: no body is fetched, except the last row's when the source ends
/// inside it (I5).
struct Verifier {
    rows: VecDeque<LogChunkMeta>,
    hasher: sha2::Sha256,
    /// Bytes of the current row read so far.
    partial: Vec<u8>,
}

impl Verifier {
    fn new(part: Vec<LogChunkMeta>) -> Self {
        use sha2::Digest;
        Self {
            rows: part.into_iter().filter(|r| r.len > 0).collect(),
            hasher: sha2::Sha256::new(),
            partial: Vec::new(),
        }
    }

    /// Feed `out`: the number of its leading bytes the stored part covers
    /// (the rest is new), or `Err` at a difference.
    fn check(&mut self, out: &[u8]) -> Result<usize, ()> {
        use sha2::Digest;
        let mut i = 0;
        while i < out.len() {
            let Some(row) = self.rows.front() else {
                return Ok(i);
            };
            let want = row.len as usize - self.partial.len();
            let take = want.min(out.len() - i);
            self.hasher.update(&out[i..i + take]);
            self.partial.extend_from_slice(&out[i..i + take]);
            i += take;
            if self.partial.len() == row.len as usize {
                let digest = std::mem::take(&mut self.hasher).finalize().to_vec();
                let row = self.rows.pop_front().expect("front");
                if row.digest.as_deref() != Some(digest.as_slice()) {
                    return Err(());
                }
                self.partial.clear();
            }
        }
        Ok(out.len())
    }

    /// The source ended inside a stored row.
    fn ends_inside(&self) -> bool {
        !self.partial.is_empty() || !self.rows.is_empty()
    }

    /// The row the source ended inside and what was read of it.
    fn inside(&self) -> Option<(&LogChunkMeta, &[u8])> {
        self.rows.front().map(|r| (r, self.partial.as_slice()))
    }
}

/// Drain one log source into the pipeline, for tests and tools: under a tail
/// lease this helper takes, after giving the stream's unit a ledger (with
/// no values) if it has none, redacted with the unit's values. Every byte
/// is appended and a clean end completes the stream. Returns the bytes
/// appended (after redaction).
pub async fn pump_log_stream(
    mut chunks: Box<dyn LogChunks>,
    logs: &LogService,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
) -> Result<u64, LogTailError> {
    let stream = LogStreamId::new(run, step, attempt);
    let unit = stream.unit();
    let values = match logs.load_values(&unit).await {
        Ok(v) => v,
        Err(LedgerError::NotLedgerComplete(NotStoredReason::NoLedger)) => {
            logs.record_launch_values(&unit, None, Vec::new(), Timestamp(0))
                .await?
        }
        Err(e) => return Err(e.into()),
    };
    let resource = tail_lease_resource(run, step, attempt);
    let owner = "pump";
    let lease = logs
        .db()
        .lease(&resource, owner, TAIL_LEASE_TTL_MS)
        .await
        .map_err(LogError::Db)?;
    let writer = LogWriter::holding(
        LogLease::Tail,
        &lease
            .held_by(&resource, owner)
            .ok_or(LogError::Db(scarab_engine::DbError::NotLeader))?,
    );
    let mut redactor = crate::logs::StreamRedactor::new(values.clone(), false);
    let mut appended = 0u64;
    let mut out = Vec::new();
    while let Some(chunk) = chunks.next_chunk().await? {
        out.extend(redactor.push(&chunk));
        if out.len() >= TAIL_CHUNK_BYTES {
            logs.append_output(&stream, &out, values.gen(), &writer, None, Timestamp(0))
                .await?;
            appended += out.len() as u64;
            out.clear();
        }
    }
    out.extend(redactor.finish_confirmed());
    if !out.is_empty() {
        logs.append_output(&stream, &out, values.gen(), &writer, None, Timestamp(0))
            .await?;
        appended += out.len() as u64;
    }
    Ok(appended)
}

/// The lease one replica holds to run the ledger sweep.
pub const LEDGER_SWEEP_LEASE: &str = "ledger-sweep";

/// How often the ledger sweep runs.
pub const LEDGER_SWEEP_EVERY: Duration = Duration::from_secs(30);

/// The longest the ledger sweep waits on one unit's revocations.
pub const LEDGER_REVOKE_TIMEOUT: Duration = Duration::from_secs(20);

/// The longest one ledger sweep pass runs; the units it did not reach are
/// the next pass's.
pub const LEDGER_SWEEP_PASS_BOUND: Duration = Duration::from_secs(300);

/// The ledger sweep lease's TTL: longer than a pass, so a pass in progress
/// is never taken over.
const LEDGER_SWEEP_LEASE_TTL_MS: i64 = 360_000;

/// Units one page of a ledger sweep pass reads.
const LEDGER_SWEEP_PAGE: u32 = 100;

/// What one ledger sweep pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LedgerSweep {
    /// Ledgers purged.
    pub purged: usize,
    /// Units whose revocation failed or timed out: each waits out its
    /// backoff before it is tried again.
    pub failed: usize,
}

/// One ledger sweep pass (I7, I8): the due units, oldest first, skipping
/// units whose revocation is backing off, page after page. Each unit's
/// minted tokens not revoked yet are revoked under
/// [`LEDGER_REVOKE_TIMEOUT`]; a unit whose revocation succeeded is purged;
/// one that failed or timed out backs off (one minute, doubling to an
/// hour) and is purged anyway once past its hard cap (logging where, never
/// the tokens). A unit in backoff is left out of the next page, so units
/// that keep failing never hide the ones behind them. The pass ends when a
/// page brings no unit it has not seen, or at [`LEDGER_SWEEP_PASS_BOUND`].
pub async fn sweep_ledgers(
    logs: &LogService,
    forge: &dyn scarab_forge::ForgePort,
    now: Timestamp,
) -> LedgerSweep {
    let deadline = tokio::time::Instant::now() + LEDGER_SWEEP_PASS_BOUND;
    let mut out = LedgerSweep::default();
    let mut seen: HashSet<LogStreamId> = HashSet::new();
    while tokio::time::Instant::now() < deadline {
        let due = match logs
            .db()
            .log_units_to_purge(now, scarab_engine::LEDGER_HARD_CAP_MS, LEDGER_SWEEP_PAGE)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(error = %e, "log ledger: the purge query failed");
                break;
            }
        };
        let full = due.len() >= LEDGER_SWEEP_PAGE as usize;
        let fresh: Vec<_> = due
            .into_iter()
            .filter(|c| seen.insert(c.unit.clone()))
            .collect();
        if fresh.is_empty() {
            break;
        }
        for c in fresh {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            sweep_unit(logs, forge, &c, now, &mut out).await;
        }
        if !full {
            break;
        }
    }
    out
}

/// Revoke and purge one due unit (see [`sweep_ledgers`]).
async fn sweep_unit(
    logs: &LogService,
    forge: &dyn scarab_forge::ForgePort,
    c: &scarab_engine::PurgeCandidate,
    now: Timestamp,
    out: &mut LedgerSweep,
) {
    if c.row.revoke_attempted_at.is_none() {
        let revoked = tokio::time::timeout(
            LEDGER_REVOKE_TIMEOUT,
            crate::forge_token::revoke_ledger_generations(
                logs,
                forge,
                &c.unit,
                crate::forge_token::RevokeScope::All,
                now,
            ),
        )
        .await
        .unwrap_or(false);
        if revoked {
            let _ = logs.db().mark_log_unit_revoked(&c.unit, now).await;
        } else if !c.past_cap {
            out.failed += 1;
            if let Err(e) = logs.db().record_log_unit_revoke_failure(&c.unit, now).await {
                tracing::debug!(error = %e, "log ledger: recording a failed revocation failed");
            }
            return;
        } else {
            tracing::warn!(
                run = %c.unit.run.0, step = %c.unit.step.0, attempt = %c.unit.attempt.0,
                "log ledger: purging a unit whose forge tokens could not all be revoked; \
                 they expire on their own"
            );
        }
    }
    if let Err(e) = logs.db().purge_log_unit(&c.unit).await {
        tracing::debug!(error = %e, "log ledger: purge failed; retried");
    } else {
        out.purged += 1;
        logs.forget_values(&c.unit);
    }
}

/// Run the ledger sweep every [`LEDGER_SWEEP_EVERY`] while this process
/// (`owner`) holds [`LEDGER_SWEEP_LEASE`]: one replica sweeps. Its own task,
/// off the scheduler's tick, so a forge that does not answer delays
/// nothing else.
pub fn spawn_ledger_sweeper(
    logs: Arc<LogService>,
    forge: Arc<dyn scarab_forge::ForgePort>,
    owner: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let held = matches!(
                logs.db().lease(LEDGER_SWEEP_LEASE, &owner, LEDGER_SWEEP_LEASE_TTL_MS).await,
                Ok(l) if l.owner == owner
            );
            if held {
                let pass = sweep_ledgers(&logs, forge.as_ref(), Timestamp(now_ms())).await;
                if pass.failed > 0 {
                    tracing::info!(
                        purged = pass.purged,
                        failed = pass.failed,
                        "log ledger sweep: some revocations failed; each is retried after its backoff"
                    );
                }
            }
            tokio::time::sleep(LEDGER_SWEEP_EVERY).await;
        }
    })
}

/// Report a failed read. The common benign case is a step Pod that has not
/// started its container yet: debug, unless it has outlasted
/// [`NOT_READY_GRACE`] (c653742).
fn report_live_error(state: &Mutex<TailState>, fence: &LogStreamId, msg: &str) {
    let now = Instant::now();
    let (class, stuck_for) = {
        let mut st = state.lock().unwrap();
        if is_pod_not_ready(msg) {
            let entry = st.not_ready.entry(fence.clone()).or_insert(NotReady {
                since: now,
                last_warn: None,
            });
            let stuck_for = now.duration_since(entry.since);
            let class = classify_tail_error(
                msg,
                stuck_for,
                entry.last_warn.map(|w| now.duration_since(w)),
            );
            if class == TailErrorClass::StuckNotReady {
                entry.last_warn = Some(now);
            }
            (class, stuck_for)
        } else {
            st.not_ready.remove(fence);
            (TailErrorClass::Failed, Duration::ZERO)
        }
    };
    let (run, step) = (&fence.run.0, &fence.step.0);
    match class {
        TailErrorClass::Quiet => {
            tracing::debug!(%run, %step, error = %msg, "log tail: not ready yet, backing off");
        }
        TailErrorClass::StuckNotReady => tracing::warn!(
            %run, %step, not_ready_for_s = stuck_for.as_secs(), error = %msg,
            "log tail: step Pod still has not started its container — no logs will appear; \
             check its init containers / image pull"
        ),
        TailErrorClass::Failed => {
            tracing::warn!(%run, %step, error = %msg, "log tail: read failed; retrying");
        }
    }
}

/// How loudly a log-tail error should be reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailErrorClass {
    Quiet,
    StuckNotReady,
    Failed,
}

/// Decide how loudly to report a log-tail error (c653742).
fn classify_tail_error(
    err: &str,
    not_ready_for: Duration,
    since_last_warn: Option<Duration>,
) -> TailErrorClass {
    if !is_pod_not_ready(err) {
        return TailErrorClass::Failed;
    }
    // A Pod waiting for a node is not wedged (git-bug 1dee2f3).
    if err.contains("PodScheduling") || not_ready_for < NOT_READY_GRACE {
        return TailErrorClass::Quiet;
    }
    match since_last_warn {
        None => TailErrorClass::StuckNotReady,
        Some(d) if d >= STUCK_WARN_INTERVAL => TailErrorClass::StuckNotReady,
        Some(_) => TailErrorClass::Quiet,
    }
}

/// True when a log-tail error just means the step Pod has not started its
/// container yet.
fn is_pod_not_ready(err: &str) -> bool {
    const NOT_READY: [&str; 3] = ["PodInitializing", "ContainerCreating", "PodScheduling"];
    NOT_READY.iter().any(|m| err.contains(m))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        classify_tail_error, is_pod_not_ready, TailErrorClass, NOT_READY_GRACE, STUCK_WARN_INTERVAL,
    };

    const INITIALIZING: &str = "executor error: exec error: ApiError: container \"step\" in pod \
                                \"scarab-check-a1-7893ca80\" is waiting to start: PodInitializing: \
                                BadRequest";

    #[test]
    fn pre_start_states_are_quiet() {
        assert!(is_pod_not_ready(INITIALIZING));
        assert!(is_pod_not_ready(
            "container \"step\" is waiting to start: ContainerCreating"
        ));
    }

    #[test]
    fn genuine_errors_still_warn() {
        assert!(!is_pod_not_ready(
            "ApiError: pods \"x\" is forbidden: cannot get pods/log"
        ));
        assert!(!is_pod_not_ready(
            "container \"step\" is waiting to start: ImagePullBackOff"
        ));
    }

    #[test]
    fn a_slow_start_stays_quiet_inside_the_grace_window() {
        assert_eq!(
            classify_tail_error(INITIALIZING, Duration::ZERO, None),
            TailErrorClass::Quiet
        );
        assert_eq!(
            classify_tail_error(INITIALIZING, NOT_READY_GRACE, None),
            TailErrorClass::StuckNotReady
        );
    }

    #[test]
    fn a_stuck_pod_is_warned_about_periodically_not_every_retry() {
        let stuck = NOT_READY_GRACE + Duration::from_secs(600);
        assert_eq!(
            classify_tail_error(INITIALIZING, stuck, Some(Duration::from_secs(3))),
            TailErrorClass::Quiet
        );
        assert_eq!(
            classify_tail_error(INITIALIZING, stuck, Some(STUCK_WARN_INTERVAL)),
            TailErrorClass::StuckNotReady
        );
    }
}
