//! # scarab-testkit — fakes for classical + deterministic-simulation testing
//!
//! Pure crate (depends only on the pure `scarab-engine` domain + `async-trait`).
//! These fakes are the **DST substrate**: an engine wired to `FakeClock` +
//! `InMemoryDb` + `FakeExecutor` runs entirely in virtual time with no real
//! I/O, so a test can replay a fixed schedule and assert exact behaviour, or
//! inject faults ("this handle dies") to explore recovery paths.
//!
//! `FakeClock` is functional (virtual time must actually advance). The `Db`
//! and `Executor` fakes carry the scaffolding a test configures, while the
//! port method bodies remain stubs for this skeleton.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, Lease};
use scarab_engine::{
    Clock, Db, DbError, EventKind, ExecError, Executor, RunId, RunStatus, StepId, StepRun,
    Timestamp,
};

// ---------------------------------------------------------------------------
// FakeClock — manually-advanced virtual time
// ---------------------------------------------------------------------------

/// A clock whose "now" only moves when a test calls [`FakeClock::advance`].
pub struct FakeClock {
    millis: AtomicI64,
}

impl FakeClock {
    /// Start virtual time at `start_millis`.
    pub fn new(start_millis: i64) -> Self {
        Self {
            millis: AtomicI64::new(start_millis),
        }
    }

    /// Advance virtual time by `delta_millis`.
    pub fn advance(&self, delta_millis: i64) {
        self.millis.fetch_add(delta_millis, Ordering::SeqCst);
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new(0)
    }
}

#[async_trait]
impl Clock for FakeClock {
    async fn now(&self) -> Timestamp {
        Timestamp(self.millis.load(Ordering::SeqCst))
    }
}

// ---------------------------------------------------------------------------
// InMemoryDb — in-process durable store stand-in
// ---------------------------------------------------------------------------

#[derive(Default)]
struct InMemoryState {
    /// The append-only event log.
    events: Vec<EventKind>,
    /// Steps a subsequent `claim_ready_steps` will hand out.
    ready: Vec<StepRun>,
    /// Current run statuses — the "state table" the real store keeps.
    runs: HashMap<RunId, RunStatus>,
}

/// An in-memory [`Db`] for tests. Holds the same shape of state a real store
/// would: an append-only event log, a ready queue, and a run-status table that
/// enforces optimistic concurrency on transitions — the in-process stand-in for
/// the version-stamped `UPDATE … WHERE status = $from` the Postgres adapter runs.
pub struct InMemoryDb {
    state: Mutex<InMemoryState>,
}

impl InMemoryDb {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(InMemoryState::default()),
        }
    }

    /// Seed the ready queue a subsequent `claim_ready_steps` would return.
    pub fn seed_ready(&self, steps: Vec<StepRun>) {
        self.state.lock().unwrap().ready = steps;
    }

    /// Snapshot of the append-only event log, in append order.
    pub fn events(&self) -> Vec<EventKind> {
        self.state.lock().unwrap().events.clone()
    }

    /// The last recorded status for `run`, if any transition has been recorded.
    pub fn run_status(&self, run: &RunId) -> Option<RunStatus> {
        self.state.lock().unwrap().runs.get(run).copied()
    }
}

impl Default for InMemoryDb {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Db for InMemoryDb {
    async fn claim_ready_steps(&self, limit: u32) -> Result<Vec<StepRun>, DbError> {
        let mut st = self.state.lock().unwrap();
        let take = (limit as usize).min(st.ready.len());
        Ok(st.ready.drain(..take).collect())
    }

    async fn record_transition(
        &self,
        run: &RunId,
        from: RunStatus,
        to: RunStatus,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        // A run first seen here is assumed `Pending`; the recorded `from` must
        // match the current status or the transition is a stale/duplicate write
        // (e.g. a crashed worker re-driving) and is rejected as a conflict.
        let current = st.runs.get(run).copied().unwrap_or(RunStatus::Pending);
        if current != from {
            return Err(DbError::Conflict);
        }
        st.runs.insert(run.clone(), to);
        Ok(())
    }

    async fn append_event(&self, event: &EventKind) -> Result<(), DbError> {
        self.state.lock().unwrap().events.push(event.clone());
        Ok(())
    }

    async fn lease(&self, _step: &StepId, owner: &str, ttl_ms: i64) -> Result<Lease, DbError> {
        Ok(Lease {
            owner: owner.to_string(),
            expires_at: Timestamp(ttl_ms),
        })
    }
}

// ---------------------------------------------------------------------------
// FakeExecutor — scriptable execution backend
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeExecState {
    /// Scripted outcomes a test wants `poll` to return, in order.
    scripted: Vec<ExecState>,
    /// Handles the test has declared "dead" (backend lost them).
    dead: Vec<ExecHandle>,
}

/// An [`Executor`] whose behaviour a test scripts: it can be told that a given
/// handle fails or dies, driving the engine's retry / recovery paths.
pub struct FakeExecutor {
    #[allow(dead_code)]
    inner: Mutex<FakeExecState>,
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FakeExecState::default()),
        }
    }

    /// Push the next outcome `poll` should report.
    pub fn script_outcome(&self, state: ExecState) {
        self.inner.lock().unwrap().scripted.push(state);
    }

    /// Declare that `handle` has died (the backend lost it).
    pub fn kill(&self, handle: ExecHandle) {
        self.inner.lock().unwrap().dead.push(handle);
    }
}

impl Default for FakeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for FakeExecutor {
    async fn launch(&self, _step: &StepRun) -> Result<ExecHandle, ExecError> {
        unimplemented!("FakeExecutor::launch")
    }

    async fn poll(&self, _handle: &ExecHandle) -> Result<ExecState, ExecError> {
        unimplemented!("FakeExecutor::poll")
    }

    async fn cancel(&self, _handle: &ExecHandle) -> Result<(), ExecError> {
        unimplemented!("FakeExecutor::cancel")
    }
}
