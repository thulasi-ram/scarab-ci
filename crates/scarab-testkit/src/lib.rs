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
#[allow(dead_code)] // fields are read once the in-memory logic is filled in.
struct InMemoryState {
    events: Vec<EventKind>,
    ready: Vec<StepRun>,
}

/// An in-memory [`Db`] for tests. Holds the same shape of state a real store
/// would (event log + a ready queue). Port methods are stubs for now.
pub struct InMemoryDb {
    #[allow(dead_code)]
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
}

impl Default for InMemoryDb {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Db for InMemoryDb {
    async fn claim_ready_steps(&self, _limit: u32) -> Result<Vec<StepRun>, DbError> {
        unimplemented!("InMemoryDb::claim_ready_steps")
    }

    async fn record_transition(
        &self,
        _run: &RunId,
        _from: RunStatus,
        _to: RunStatus,
    ) -> Result<(), DbError> {
        unimplemented!("InMemoryDb::record_transition")
    }

    async fn append_event(&self, _event: &EventKind) -> Result<(), DbError> {
        unimplemented!("InMemoryDb::append_event")
    }

    async fn lease(&self, _step: &StepId, _owner: &str, _ttl_ms: i64) -> Result<Lease, DbError> {
        unimplemented!("InMemoryDb::lease")
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
