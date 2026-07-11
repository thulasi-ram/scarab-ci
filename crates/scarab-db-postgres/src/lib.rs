//! Postgres adapter for the [`scarab_engine::Db`] port.
//!
//! Adapter crate: pairs the pure `scarab-engine` domain with the `sqlx`
//! infra crate. All port methods are stubs.

use async_trait::async_trait;
use scarab_engine::{
    Db, DbError, EventKind, RunId, RunStatus, StepId, StepRun,
};
use scarab_engine::ports::Lease;

/// A Postgres-backed [`Db`]. Holds an optional connection pool so the
/// composition root can construct it without performing I/O.
pub struct PostgresDb {
    #[allow(dead_code)] // wired at composition time; read once queries land.
    pool: Option<sqlx::PgPool>,
}

impl PostgresDb {
    /// Construct without connecting (pool wired later).
    pub fn new() -> Self {
        Self { pool: None }
    }

    /// Construct from an existing connection pool.
    pub fn with_pool(pool: sqlx::PgPool) -> Self {
        Self { pool: Some(pool) }
    }
}

impl Default for PostgresDb {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Db for PostgresDb {
    async fn claim_ready_steps(&self, _limit: u32) -> Result<Vec<StepRun>, DbError> {
        // TODO: SELECT ... FOR UPDATE SKIP LOCKED against the steps table.
        unimplemented!("PostgresDb::claim_ready_steps")
    }

    async fn record_transition(
        &self,
        _run: &RunId,
        _from: RunStatus,
        _to: RunStatus,
    ) -> Result<(), DbError> {
        unimplemented!("PostgresDb::record_transition")
    }

    async fn append_event(&self, _event: &EventKind) -> Result<(), DbError> {
        unimplemented!("PostgresDb::append_event")
    }

    async fn lease(&self, _step: &StepId, _owner: &str, _ttl_ms: i64) -> Result<Lease, DbError> {
        unimplemented!("PostgresDb::lease")
    }
}
