//! Postgres adapter for the [`scarab_engine::Db`] port.
//!
//! Adapter crate: pairs the pure `scarab-engine` domain with the `sqlx` infra
//! crate (the domain core stays pure — ADR-0016). This module owns the durable
//! schema (see `migrations/`), the expand-contract migration harness, and the
//! Postgres implementation of the state tables + append-only event log +
//! transactional outbox (ADR-0003, 0013, 0022).
//!
//! Timestamps cross the boundary as `Timestamp(i64)` unix-millis and are stored
//! as `BIGINT`, so no date/time crate leaks toward the domain.
//!
//! Scope note: `claim_ready_steps` (SELECT … FOR UPDATE SKIP LOCKED) and the
//! outbox *dispatcher* land with the scheduler slice; this crate establishes the
//! schema and round-trips every table through the adapter.

use async_trait::async_trait;
use serde_json::Value;
use sqlx::migrate::Migrator;
use sqlx::{PgPool, Row};

use scarab_engine::ports::{LeaderTerm, Lease, MintLaunch};
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, ConcurrencyPolicy, Db, DbError, EventKind, EventPayload,
    FailureKind, LogChunkMeta, OutboxId, OutboxMessage, RunId, RunService, RunStatus, RunSummary,
    ServiceStatus, StepId, StepRun, StepSpec, StepStatus, Timestamp,
};
use scarab_forge::{
    ForgeConnection, ForgeConnectionStore, ForgeKind, OrgAssignView, RegistryError, RepoRef,
    ResolvedRepo,
};
use scarab_project::ProjectError;

/// `EnvironmentGovernanceStore` (ADR-0071 §2, §3, §8): baseline, eligibility,
/// overlays and grants, and the one-snapshot read admission makes.
mod environment_governance;
/// `LibraryStore` (ADR-0076): installed libraries, their versions, yanks
/// and the sources each org trusts with forge credentials.
mod libraries;
/// `OrgEnvironmentStore` (ADR-0071): the org-level Environment.
mod org_environments;
/// `PluginStore` (ADR-0072): PluginInstallations, the kill switch, their
/// PluginConnections and the broker journal.
mod plugins;
/// `PolicyStore` (ADR-0075): Policies, their versions, and PolicyDecisions.
mod policies;
mod schedules;
/// Step-up confirmation (ADR-0073): `PasskeyStore`, `ChallengeStore`,
/// `StepUpStore` and the append-only `OrgSecurityEventLog`.
mod step_up;
pub use step_up::{step_up_request_lock_key, BindingRow};

/// The embedded, ordered set of forward-only migrations. `MIGRATOR.run(pool)`
/// applies all pending ones (tracked in `_sqlx_migrations`); tests can also walk
/// `MIGRATOR.iter()` to apply schema versions one at a time.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// A Postgres-backed [`Db`]. Always connected: Postgres is mandatory for
/// every serving role (ADR-0048) — the unconnected/API-only construction was
/// deleted, not guarded.
pub struct PostgresDb {
    pool: PgPool,
    /// The bound on every leader-fenced transaction (git-bug aaf4e3e):
    /// [`LEADER_TX_BOUND_MS`](scarab_engine::LEADER_TX_BOUND_MS) unless a
    /// test shortens it. See [`PostgresDb::begin_fenced`].
    leader_tx_bound: std::time::Duration,
}

/// How long a lease takeover waits for the holder's in-flight fenced
/// transaction before it reports the holder and leaves the takeover to the
/// caller's next tick. Only a takeover of an EXPIRED lease waits at all; a
/// follower that finds the lease live never does (git-bug aaf4e3e).
const TAKEOVER_WAIT: &str = "1s";

impl PostgresDb {
    /// Construct from an existing connection pool.
    pub fn with_pool(pool: PgPool) -> Self {
        Self {
            pool,
            leader_tx_bound: std::time::Duration::from_millis(scarab_engine::LEADER_TX_BOUND_MS),
        }
    }

    /// The connection pool, for tests that seed rows the ports never write
    /// (a Principal with a chosen id, say).
    pub fn pool_for_tests(&self) -> &PgPool {
        &self.pool
    }

    /// Override the leader-fenced transaction bound (tests that stall one on
    /// purpose).
    pub fn with_leader_tx_bound(mut self, bound: std::time::Duration) -> Self {
        self.leader_tx_bound = bound;
        self
    }

    /// Begin a leader-fenced transaction (git-bug aaf4e3e): bound it, then
    /// check that `leader` still holds its lease term and hold the lease row
    /// `FOR SHARE` until the transaction ends. A takeover updates that row,
    /// so it waits for this transaction: whatever the caller writes in it is
    /// refused here ([`DbError::NotLeader`]) or committed before the
    /// takeover. A share lock waiting behind a takeover in progress sees the
    /// new owner when it is granted, and is refused.
    ///
    /// The bound (`lock_timeout`, `statement_timeout` and
    /// `idle_in_transaction_session_timeout`, all `leader_tx_bound`, local
    /// to the transaction) is what keeps the lease row from outliving its
    /// holder: a holder stopped mid-transaction has it rolled back by
    /// Postgres — a statement waiting on a lock is cancelled, an idle one
    /// has its session ended — so a peer takes the lease over within TTL +
    /// bound and the stopped holder's writes never land. A live holder that
    /// hits the bound gets [`DbError::FenceTimeout`] (see [`db_err`]).
    async fn begin_fenced(
        &self,
        leader: &LeaderTerm,
    ) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, DbError> {
        let mut tx = self.pool().begin().await.map_err(db_err)?;
        let bound = format!("{}ms", self.leader_tx_bound.as_millis().max(1));
        sqlx::query(
            "SELECT set_config('lock_timeout', $1, true),
                    set_config('statement_timeout', $1, true),
                    set_config('idle_in_transaction_session_timeout', $1, true)",
        )
        .bind(&bound)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        let held = sqlx::query(
            "SELECT 1 FROM leases WHERE resource = $1 AND owner = $2 AND term = $3 FOR SHARE",
        )
        .bind(leader.resource())
        .bind(leader.owner())
        .bind(leader.term())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        match held {
            Some(_) => Ok(tx),
            None => Err(DbError::NotLeader),
        }
    }

    /// [`begin_fenced`](Self::begin_fenced) with a leader, a plain
    /// transaction without one.
    async fn begin_maybe_fenced(
        &self,
        leader: Option<&LeaderTerm>,
    ) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, DbError> {
        match leader {
            Some(leader) => self.begin_fenced(leader).await,
            None => self.pool().begin().await.map_err(db_err),
        }
    }

    /// Connect a pool to `url`.
    pub async fn connect(url: &str) -> Result<Self, DbError> {
        let pool = PgPool::connect(url).await.map_err(db_err)?;
        Ok(Self::with_pool(pool))
    }

    /// Apply all pending migrations (the production expand-contract path).
    pub async fn migrate(&self) -> Result<(), DbError> {
        MIGRATOR
            .run(self.pool())
            .await
            .map_err(|e| DbError::Other(e.to_string()))
    }

    fn pool(&self) -> &PgPool {
        &self.pool
    }

    // --- read helpers ------------------------------------------------------

    /// Current status of a step, if it exists.
    pub async fn step_status(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<StepStatus>, DbError> {
        let row = sqlx::query("SELECT status FROM step_runs WHERE run_id = $1 AND step_id = $2")
            .bind(&run.0)
            .bind(&step.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        row.map(|r| step_status_from_str(r.get::<String, _>("status")))
            .transpose()
    }

    /// All attempts of a step, in mint order.
    ///
    /// Ordered by the numeric `a{n}` id suffix ALONE (`a1`,`a2`,…; minted
    /// monotonic under the step — see the scheduler), so `.last()` is the
    /// frontier that anchors `?attempt=` reads and the settle-path frontier
    /// guard. Never by `started_at` (git-bug 90d767d): it is not unique (the
    /// test `FakeClock` ties, same-millisecond real minting), it can run
    /// BACKWARDS across a leader failover (a retried `a2` stamped before
    /// `a1`), and a gate-wait Attempt is minted with a frozen, earlier start by
    /// design. It must be numeric, not lexical: lexical order puts `a10` before
    /// `a2`. The in-memory `Db` (scarab-testkit) mirrors this exact order.
    pub async fn attempts(&self, run: &RunId, step: &StepId) -> Result<Vec<Attempt>, DbError> {
        attempts_on(self.pool(), run, step).await
    }

    /// Kinds of the currently-pending (undispatched) outbox rows for a run.
    pub async fn pending_outbox_kinds(&self, run: &RunId) -> Result<Vec<String>, DbError> {
        let rows = sqlx::query(
            "SELECT kind FROM outbox WHERE run_id = $1 AND dispatched_at IS NULL ORDER BY id",
        )
        .bind(&run.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("kind"))
            .collect())
    }
}

#[async_trait]
impl Db for PostgresDb {
    async fn claim_ready_steps(
        &self,
        leader: &LeaderTerm,
        limit: u32,
        mint: MintLaunch<'_>,
    ) -> Result<Vec<StepRun>, DbError> {
        // Atomic dequeue: the inner SELECT locks up to `limit` ready rows with
        // FOR UPDATE SKIP LOCKED (concurrent claimers skip each other's locked
        // rows), and the outer UPDATE flips them to `running` in the same
        // statement. After commit the rows are no longer `ready`, so a later
        // claim cannot hand out the same step — no double-dispatch.
        //
        // Leader-only (git-bug aaf4e3e): the fence, the claim and every
        // claimed step's Attempt, events and launch intent commit together
        // (git-bug 0f29112), so no step is ever `running` without them.
        let mut tx = self.begin_fenced(leader).await?;
        let rows = sqlx::query(
            "UPDATE step_runs
             SET status = 'running',
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE (run_id, step_id) IN (
                 SELECT run_id, step_id FROM step_runs
                 WHERE status = 'ready'
                 ORDER BY created_at, step_id
                 FOR UPDATE SKIP LOCKED
                 LIMIT $1
             )
             RETURNING run_id, step_id, status, needs, gate_kind",
        )
        .bind(limit as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;

        let mut claimed = Vec::with_capacity(rows.len());
        for r in rows {
            let run = RunId(r.get::<String, _>("run_id"));
            let step = StepId(r.get::<String, _>("step_id"));
            let status = step_status_from_str(r.get::<String, _>("status"))?;
            let needs = needs_from_value(r.get::<Value, _>("needs"))?;
            let gate_kind = r.get::<Option<String>, _>("gate_kind");
            let attempts = attempts_on(&mut *tx, &run, &step).await?;
            let step_run = StepRun {
                run,
                step,
                status,
                attempts,
                needs,
                gate_kind,
            };
            let launch = mint(&step_run)?;
            insert_attempt(&mut tx, &step_run.run, &step_run.step, &launch.attempt).await?;
            for event in &launch.events {
                insert_event(&mut *tx, event).await?;
            }
            for msg in &launch.outbox {
                insert_outbox(&mut *tx, msg).await?;
            }
            claimed.push(step_run);
        }
        tx.commit().await.map_err(db_err)?;
        Ok(claimed)
    }

    async fn create_run(
        &self,
        run: &RunId,
        ir_version: u32,
        event_schema_version: u32,
        at: Timestamp,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $5)",
        )
        .bind(&run.0)
        .bind(run_status_str(RunStatus::Pending))
        .bind(ir_version as i32)
        .bind(event_schema_version as i32)
        .bind(at.0)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn begin_run_creation(
        &self,
        run: &RunId,
        ir_version: u32,
        event_schema_version: u32,
        at: Timestamp,
    ) -> Result<(), DbError> {
        insert_creating_run(self.pool(), &run.0, ir_version, event_schema_version, at.0)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn finish_run_creation(&self, run: &RunId) -> Result<(), DbError> {
        sqlx::query("UPDATE runs SET creating = FALSE WHERE id = $1")
            .bind(&run.0)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn run_creation_unfinished(&self, run: &RunId) -> Result<bool, DbError> {
        let row = sqlx::query("SELECT creating FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        Ok(row.is_some_and(|r| r.get::<bool, _>("creating")))
    }

    async fn unfinished_creations_before(
        &self,
        created_before: Timestamp,
    ) -> Result<Vec<RunId>, DbError> {
        let rows = sqlx::query(
            "SELECT id FROM runs WHERE creating AND status = 'pending' AND created_at < $1
             ORDER BY created_at",
        )
        .bind(created_before.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| RunId(r.get::<String, _>("id")))
            .collect())
    }

    async fn create_step_run(
        &self,
        run: &RunId,
        step: &StepId,
        spec: Option<&StepSpec>,
        needs: &[StepId],
        at: Timestamp,
    ) -> Result<(), DbError> {
        let spec_json = spec
            .map(|s| serde_json::to_value(s).map_err(|e| DbError::Other(e.to_string())))
            .transpose()?;
        let needs_json = needs_to_value(needs);
        sqlx::query(
            "INSERT INTO step_runs (run_id, step_id, status, spec, needs, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $6)",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(step_status_str(StepStatus::Pending))
        .bind(spec_json)
        .bind(needs_json)
        .bind(at.0)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn store_run_ir(&self, run: &RunId, ir: &serde_json::Value) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE runs
             SET ir = $2, updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(&run.0)
        .bind(ir)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_ir(&self, run: &RunId) -> Result<Option<serde_json::Value>, DbError> {
        let row = sqlx::query("SELECT ir FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        // `ir` is itself nullable, so unwrap both the missing-row and NULL cases.
        Ok(row.and_then(|r| r.get::<Option<Value>, _>("ir")))
    }

    async fn set_run_params(
        &self,
        run: &RunId,
        params: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), DbError> {
        let json = serde_json::to_value(params).map_err(|e| DbError::Other(e.to_string()))?;
        sqlx::query(
            "UPDATE runs
             SET params = $2, updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(&run.0)
        .bind(json)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_params(
        &self,
        run: &RunId,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError> {
        let row = sqlx::query("SELECT params FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        match row.and_then(|r| r.get::<Option<Value>, _>("params")) {
            Some(v) => serde_json::from_value(v).map_err(|e| DbError::Other(e.to_string())),
            None => Ok(std::collections::BTreeMap::new()),
        }
    }

    async fn set_run_event_facts(
        &self,
        run: &RunId,
        facts: &serde_json::Map<String, Value>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE runs
             SET event_facts = $2, updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(&run.0)
        .bind(Value::Object(facts.clone()))
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_event_facts(
        &self,
        run: &RunId,
    ) -> Result<serde_json::Map<String, Value>, DbError> {
        let row = sqlx::query("SELECT event_facts FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        match row.and_then(|r| r.get::<Option<Value>, _>("event_facts")) {
            Some(Value::Object(m)) => Ok(m),
            Some(other) => Err(DbError::Other(format!(
                "runs.event_facts is not an object: {other}"
            ))),
            None => Ok(serde_json::Map::new()),
        }
    }

    async fn set_run_deploy_context(
        &self,
        run: &RunId,
        ctx: &scarab_engine::DeployContext,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE runs SET deploy_org = $2, deploy_project = $3, deploy_environment = $4,
                 deploy_git_ref = $5, deploy_locked_out = $6,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(&run.0)
        .bind(&ctx.org)
        .bind(&ctx.project)
        .bind(&ctx.environment)
        .bind(&ctx.git_ref)
        .bind(ctx.locked_out)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_deploy_context(
        &self,
        run: &RunId,
    ) -> Result<Option<scarab_engine::DeployContext>, DbError> {
        let row = sqlx::query(
            "SELECT deploy_org, deploy_project, deploy_environment, deploy_git_ref, deploy_locked_out
             FROM runs WHERE id = $1",
        )
        .bind(&run.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row.and_then(|r| {
            // The four scope columns are set together (or all NULL for a
            // non-deploy run); `deploy_locked_out` defaults false.
            match (
                r.get::<Option<String>, _>("deploy_org"),
                r.get::<Option<String>, _>("deploy_project"),
                r.get::<Option<String>, _>("deploy_environment"),
                r.get::<Option<String>, _>("deploy_git_ref"),
            ) {
                (Some(org), Some(project), Some(environment), Some(git_ref)) => {
                    Some(scarab_engine::DeployContext {
                        org,
                        project,
                        environment,
                        git_ref,
                        locked_out: r.get::<bool, _>("deploy_locked_out"),
                    })
                }
                _ => None,
            }
        }))
    }

    async fn mark_deploy_run(&self, run: &RunId, steps: &[StepId]) -> Result<(), DbError> {
        let steps: Vec<&str> = steps.iter().map(|s| s.0.as_str()).collect();
        sqlx::query("UPDATE runs SET is_deploy_run = TRUE, deploy_steps = $2 WHERE id = $1")
            .bind(&run.0)
            .bind(&steps)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn deploy_steps_of_run(&self, run: &RunId) -> Result<Vec<StepId>, DbError> {
        let row = sqlx::query("SELECT deploy_steps FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        Ok(row
            .map(|r| r.get::<Vec<String>, _>("deploy_steps"))
            .unwrap_or_default()
            .into_iter()
            .map(StepId)
            .collect())
    }

    async fn set_step_output(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        snapshot: &str,
        identity: Option<&str>,
    ) -> Result<(), DbError> {
        // One transaction (ADR-0056): the attempt's immutable evidence copy
        // and the step's latest-evidence denormalization (+ its provenance
        // stamp) move together or not at all. The identity travels in the same
        // statement as the root it describes — a row with one and not the other
        // would be a snapshot whose content nobody can compare (ADR-0061 s8).
        let mut tx = self.pool().begin().await.map_err(db_err)?;
        sqlx::query(
            "UPDATE step_runs
             SET output_snapshot = $3,
                 output_identity = $5,
                 evidence_attempt = $4,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(snapshot)
        .bind(&attempt.0)
        .bind(identity)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            "UPDATE attempts
             SET output_snapshot = $4, output_identity = $5
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .bind(snapshot)
        .bind(identity)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn step_output_identity(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<String>, DbError> {
        // COALESCE is the fallback the port documents, in SQL: a row written
        // before ADR-0061 s8 has no identity, and comparing by its root is the
        // pre-identity behaviour — it cascades where it might have skipped.
        let row = sqlx::query(
            "SELECT COALESCE(output_identity, output_snapshot) AS cmp
             FROM step_runs WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("cmp")))
    }

    async fn step_output(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        let row =
            sqlx::query("SELECT output_snapshot FROM step_runs WHERE run_id = $1 AND step_id = $2")
                .bind(&run.0)
                .bind(&step.0)
                .fetch_optional(self.pool())
                .await
                .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("output_snapshot")))
    }

    async fn attempt_output(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<String>, DbError> {
        let row = sqlx::query(
            "SELECT output_snapshot FROM attempts
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("output_snapshot")))
    }

    async fn set_step_results(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        results: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), DbError> {
        let json = serde_json::to_value(results).map_err(|e| DbError::Other(e.to_string()))?;
        // One transaction (ADR-0056) — see `set_step_output`.
        let mut tx = self.pool().begin().await.map_err(db_err)?;
        sqlx::query(
            "UPDATE step_runs
             SET results = $3,
                 evidence_attempt = $4,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&json)
        .bind(&attempt.0)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            "UPDATE attempts SET results = $4
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .bind(&json)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn ingest_attempt_results(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        results: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), DbError> {
        let json = serde_json::to_value(results).map_err(|e| DbError::Other(e.to_string()))?;
        // One transaction with the step row locked: a finalize's status CAS
        // (which locks the same row) either committed first — the step is no
        // longer `running` here — or waits for this write. An early return
        // drops `tx`, which rolls it back.
        let mut tx = self.pool().begin().await.map_err(db_err)?;
        let status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM step_runs WHERE run_id = $1 AND step_id = $2 FOR UPDATE",
        )
        .bind(&run.0)
        .bind(&step.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        if status.as_deref() != Some(step_status_str(StepStatus::Running)) {
            return Err(DbError::Conflict);
        }
        let frontier = attempts_on(&mut *tx, run, step).await?;
        if frontier.last().map(|a| &a.id) != Some(attempt) {
            return Err(DbError::Conflict);
        }
        sqlx::query(
            "UPDATE step_runs
             SET results = $3,
                 evidence_attempt = $4,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&json)
        .bind(&attempt.0)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            "UPDATE attempts SET results = $4
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .bind(&json)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)
    }

    async fn step_results(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError> {
        let row = sqlx::query("SELECT results FROM step_runs WHERE run_id = $1 AND step_id = $2")
            .bind(&run.0)
            .bind(&step.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        match row.and_then(|r| r.get::<Option<Value>, _>("results")) {
            Some(v) => serde_json::from_value(v).map_err(|e| DbError::Other(e.to_string())),
            None => Ok(std::collections::BTreeMap::new()),
        }
    }

    async fn attempt_results(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError> {
        let row = sqlx::query(
            "SELECT results FROM attempts
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        match row.and_then(|r| r.get::<Option<Value>, _>("results")) {
            Some(v) => serde_json::from_value(v).map_err(|e| DbError::Other(e.to_string())),
            None => Ok(std::collections::BTreeMap::new()),
        }
    }

    async fn step_evidence_attempt(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<AttemptId>, DbError> {
        let row = sqlx::query(
            "SELECT evidence_attempt FROM step_runs WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row
            .and_then(|r| r.get::<Option<String>, _>("evidence_attempt"))
            .map(AttemptId))
    }

    async fn set_attempt_consumed(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        consumed: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), DbError> {
        let json = serde_json::to_value(consumed).map_err(|e| DbError::Other(e.to_string()))?;
        sqlx::query(
            "UPDATE attempts SET consumed = $4
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .bind(json)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn attempt_consumed(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<std::collections::BTreeMap<String, String>, DbError> {
        let row = sqlx::query(
            "SELECT consumed FROM attempts
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        match row.and_then(|r| r.get::<Option<Value>, _>("consumed")) {
            Some(v) => serde_json::from_value(v).map_err(|e| DbError::Other(e.to_string())),
            None => Ok(std::collections::BTreeMap::new()),
        }
    }

    async fn set_attempt_workspace_input_steps(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        steps: &[StepId],
    ) -> Result<(), DbError> {
        let ids: Vec<&str> = steps.iter().map(|s| s.0.as_str()).collect();
        let json = serde_json::to_value(ids).map_err(|e| DbError::Other(e.to_string()))?;
        sqlx::query(
            "UPDATE attempts SET workspace_input_steps = $4
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .bind(json)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn attempt_workspace_input_steps(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<Vec<StepId>>, DbError> {
        let row = sqlx::query(
            "SELECT workspace_input_steps FROM attempts
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        match row.and_then(|r| r.get::<Option<Value>, _>("workspace_input_steps")) {
            Some(v) => serde_json::from_value::<Vec<String>>(v)
                .map(|ids| Some(ids.into_iter().map(StepId).collect()))
                .map_err(|e| DbError::Other(e.to_string())),
            None => Ok(None),
        }
    }

    async fn set_step_input(
        &self,
        run: &RunId,
        step: &StepId,
        signature: Option<&str>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE step_runs
             SET input_signature = $3,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(signature)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn promote_step(
        &self,
        leader: &LeaderTerm,
        run: &RunId,
        step: &StepId,
        signature: &str,
        events: &[EventKind],
    ) -> Result<(), DbError> {
        // One fenced transaction: the CAS takes the step row's lock, the
        // signature lands only with the promotion, an early return rolls back.
        let mut tx = self.begin_fenced(leader).await?;
        let promoted = sqlx::query(
            "UPDATE step_runs
             SET status = 'ready', input_signature = $3,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND step_id = $2 AND status = 'pending'",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(signature)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if promoted == 0 {
            return Err(DbError::Conflict);
        }
        for event in events {
            insert_event(&mut *tx, event).await?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn step_input(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        let row =
            sqlx::query("SELECT input_signature FROM step_runs WHERE run_id = $1 AND step_id = $2")
                .bind(&run.0)
                .bind(&step.0)
                .fetch_optional(self.pool())
                .await
                .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("input_signature")))
    }

    async fn set_step_inputs(
        &self,
        run: &RunId,
        step: &StepId,
        inputs: &[StepId],
    ) -> Result<(), DbError> {
        let ids: Vec<&str> = inputs.iter().map(|s| s.0.as_str()).collect();
        let json = serde_json::to_value(&ids).map_err(|e| DbError::Other(e.to_string()))?;
        sqlx::query(
            "UPDATE step_runs
             SET explicit_inputs = $3,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(json)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn step_inputs(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<Vec<StepId>>, DbError> {
        let row =
            sqlx::query("SELECT explicit_inputs FROM step_runs WHERE run_id = $1 AND step_id = $2")
                .bind(&run.0)
                .bind(&step.0)
                .fetch_optional(self.pool())
                .await
                .map_err(db_err)?;
        let Some(value) = row.and_then(|r| r.get::<Option<Value>, _>("explicit_inputs")) else {
            return Ok(None);
        };
        let ids: Vec<String> =
            serde_json::from_value(value).map_err(|e| DbError::Other(e.to_string()))?;
        Ok(Some(ids.into_iter().map(StepId).collect()))
    }

    async fn set_run_concurrency(
        &self,
        run: &RunId,
        group: &str,
        policy: ConcurrencyPolicy,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE runs SET concurrency_group = $2, concurrency_policy = $3,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(&run.0)
        .bind(group)
        .bind(policy.as_str())
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_concurrency(
        &self,
        run: &RunId,
    ) -> Result<Option<(String, ConcurrencyPolicy)>, DbError> {
        let row =
            sqlx::query("SELECT concurrency_group, concurrency_policy FROM runs WHERE id = $1")
                .bind(&run.0)
                .fetch_optional(self.pool())
                .await
                .map_err(db_err)?;
        Ok(row.and_then(|r| {
            let group: Option<String> = r.get("concurrency_group");
            let policy: Option<String> = r.get("concurrency_policy");
            group.map(|g| {
                (
                    g,
                    ConcurrencyPolicy::from_wire(policy.as_deref().unwrap_or("queue")),
                )
            })
        }))
    }

    async fn acquire_slot(
        &self,
        leader: &LeaderTerm,
        group: &str,
        run: &RunId,
    ) -> Result<Option<RunId>, DbError> {
        // Serialize acquirers on this group with a row lock, so exactly one wins.
        let mut tx = self.begin_fenced(leader).await?;
        let holder: Option<String> =
            sqlx::query("SELECT holder FROM concurrency_slots WHERE group_key = $1 FOR UPDATE")
                .bind(group)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?
                .map(|r| r.get::<String, _>("holder"));

        let result = match holder {
            None => {
                sqlx::query("INSERT INTO concurrency_slots (group_key, holder) VALUES ($1, $2)")
                    .bind(group)
                    .bind(&run.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                None
            }
            Some(h) if h == run.0 => None, // already ours (idempotent)
            Some(h) => {
                // Reclaim the slot if the current holder has settled (or vanished).
                let holder_terminal = sqlx::query("SELECT status FROM runs WHERE id = $1")
                    .bind(&h)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err)?
                    .map(|r| run_status_from_str(r.get::<String, _>("status")))
                    .transpose()?
                    .map(|s| s.is_terminal())
                    .unwrap_or(true);
                if holder_terminal {
                    sqlx::query("UPDATE concurrency_slots SET holder = $2 WHERE group_key = $1")
                        .bind(group)
                        .bind(&run.0)
                        .execute(&mut *tx)
                        .await
                        .map_err(db_err)?;
                    None
                } else {
                    Some(RunId(h))
                }
            }
        };
        tx.commit().await.map_err(db_err)?;
        Ok(result)
    }

    async fn release_slot(&self, group: &str, run: &RunId) -> Result<(), DbError> {
        sqlx::query("DELETE FROM concurrency_slots WHERE group_key = $1 AND holder = $2")
            .bind(group)
            .bind(&run.0)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn set_supersede_key(&self, run: &RunId, key: &str) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE runs SET supersede_key = $2,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(&run.0)
        .bind(key)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn superseded_by(&self, run: &RunId) -> Result<Vec<RunId>, DbError> {
        // Older non-terminal runs sharing this run's key. If the run has no key,
        // the `= (subquery)` is NULL for every row → empty result.
        let rows = sqlx::query(
            "SELECT id FROM runs
             WHERE supersede_key IS NOT NULL
               AND supersede_key = (SELECT supersede_key FROM runs WHERE id = $1)
               AND id <> $1
               AND created_at < (SELECT created_at FROM runs WHERE id = $1)
               AND status NOT IN ('succeeded', 'failed', 'cancelled', 'dead_lettered')
             ORDER BY created_at",
        )
        .bind(&run.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| RunId(r.get::<String, _>("id")))
            .collect())
    }

    async fn set_run_scheduling(
        &self,
        run: &RunId,
        project: &str,
        priority: i32,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE runs SET project = $2, priority = $3,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(&run.0)
        .bind(project)
        .bind(priority)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_project(&self, run: &RunId) -> Result<Option<String>, DbError> {
        let row = sqlx::query("SELECT project FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("project")))
    }

    async fn set_run_tenant(&self, run: &RunId, org: &str, project: &str) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE runs SET tenant_org = $2, tenant_project = $3,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(&run.0)
        .bind(org)
        .bind(project)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_tenant(&self, run: &RunId) -> Result<Option<(String, String)>, DbError> {
        let row = sqlx::query("SELECT tenant_org, tenant_project FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        Ok(row.and_then(|r| {
            match (
                r.get::<Option<String>, _>("tenant_org"),
                r.get::<Option<String>, _>("tenant_project"),
            ) {
                (Some(o), Some(p)) => Some((o, p)),
                _ => None,
            }
        }))
    }

    async fn run_tenants(
        &self,
        runs: &[RunId],
    ) -> Result<std::collections::HashMap<RunId, (String, String)>, DbError> {
        if runs.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let ids: Vec<&str> = runs.iter().map(|r| r.0.as_str()).collect();
        let rows = sqlx::query(
            "SELECT id, tenant_org, tenant_project FROM runs
             WHERE id = ANY($1)
               AND tenant_org IS NOT NULL AND tenant_project IS NOT NULL",
        )
        .bind(&ids)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    RunId(r.get::<String, _>("id")),
                    (
                        r.get::<String, _>("tenant_org"),
                        r.get::<String, _>("tenant_project"),
                    ),
                )
            })
            .collect())
    }

    async fn allocate_run_number(
        &self,
        run: &RunId,
        org: &str,
        project: &str,
    ) -> Result<i64, DbError> {
        // One atomic upsert bumps the per-repo counter and hands back the number
        // to assign; concurrent creations for the same repo serialize on this
        // row rather than racing a MAX+1 scan.
        let row = sqlx::query(
            "INSERT INTO repo_run_counters (org, project, last_number) VALUES ($1, $2, 1)
             ON CONFLICT (org, project)
                 DO UPDATE SET last_number = repo_run_counters.last_number + 1
             RETURNING last_number",
        )
        .bind(org)
        .bind(project)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        let n = row.get::<i64, _>("last_number");
        sqlx::query(
            "UPDATE runs SET run_number = $2,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(&run.0)
        .bind(n)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(n)
    }

    async fn run_number(&self, run: &RunId) -> Result<Option<i64>, DbError> {
        let row = sqlx::query("SELECT run_number FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<i64>, _>("run_number")))
    }

    async fn set_run_origin(
        &self,
        run: &RunId,
        trigger_kind: &str,
        actor: Option<&str>,
        git_ref: Option<&str>,
        sha: Option<&str>,
        pr_number: Option<i64>,
        pr_base: Option<&str>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE runs SET origin_trigger_kind = $2, origin_actor = $3,
                 origin_ref = $4, origin_sha = $5, origin_pr_number = $6,
                 origin_pr_base = $7,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(&run.0)
        .bind(trigger_kind)
        .bind(actor)
        .bind(git_ref)
        .bind(sha)
        .bind(pr_number)
        .bind(pr_base)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_pr_base(&self, run: &RunId) -> Result<Option<String>, DbError> {
        let row = sqlx::query("SELECT origin_pr_base FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("origin_pr_base")))
    }

    async fn cache_record(
        &self,
        project: &str,
        key: &str,
        dir: &str,
        tree_root: &str,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        now: Timestamp,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO cache_entries
                 (project, key, dir, tree_root, saved_at, run_id, step_id, attempt)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (project, key, dir) DO UPDATE SET
                 tree_root = EXCLUDED.tree_root,
                 saved_at = EXCLUDED.saved_at,
                 run_id = EXCLUDED.run_id,
                 step_id = EXCLUDED.step_id,
                 attempt = EXCLUDED.attempt",
        )
        .bind(project)
        .bind(key)
        .bind(dir)
        .bind(tree_root)
        .bind(now.0)
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn set_run_cache_scope(
        &self,
        run: &RunId,
        scope: &scarab_engine::CacheScope,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE runs SET cache_scope = $2, cache_fallback_scope = $3, \
             cache_environment_scope = $4 WHERE id = $1",
        )
        .bind(&run.0)
        .bind(scope.own.as_deref())
        .bind(scope.fallback.as_deref())
        .bind(scope.environment.as_deref())
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_cache_scope(
        &self,
        run: &RunId,
    ) -> Result<Option<scarab_engine::CacheScope>, DbError> {
        let row = sqlx::query(
            "SELECT cache_scope, cache_fallback_scope, cache_environment_scope \
             FROM runs WHERE id = $1",
        )
        .bind(&run.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row.and_then(|r| {
            let mut own = r.get::<Option<String>, _>("cache_scope");
            let mut environment = r.get::<Option<String>, _>("cache_environment_scope");
            // A Run stored before 0083 kept `env:<name>` as its only scope:
            // its Steps before the gate get no Cache rather than that scope.
            if environment.is_none() && own.as_deref().is_some_and(|o| o.starts_with("env:")) {
                environment = own.take();
            }
            if own.is_none() && environment.is_none() {
                return None;
            }
            Some(scarab_engine::CacheScope {
                own,
                fallback: r.get::<Option<String>, _>("cache_fallback_scope"),
                environment,
            })
        }))
    }

    async fn cache_lookup(
        &self,
        project: &str,
        key: &str,
    ) -> Result<Vec<(String, String)>, DbError> {
        let rows = sqlx::query(
            "SELECT dir, tree_root FROM cache_entries
             WHERE project = $1 AND key = $2 ORDER BY dir",
        )
        .bind(project)
        .bind(key)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("dir"), r.get::<String, _>("tree_root")))
            .collect())
    }

    async fn set_run_pipeline(&self, run: &RunId, pipeline: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE runs SET pipeline = $2 WHERE id = $1")
            .bind(&run.0)
            .bind(pipeline)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn run_pipeline(&self, run: &RunId) -> Result<Option<String>, DbError> {
        let row = sqlx::query("SELECT pipeline FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("pipeline")))
    }

    async fn set_run_trigger_title(&self, run: &RunId, title: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE runs SET trigger_title = $2 WHERE id = $1")
            .bind(&run.0)
            .bind(title)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn run_trigger_title(&self, run: &RunId) -> Result<Option<String>, DbError> {
        let row = sqlx::query("SELECT trigger_title FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("trigger_title")))
    }

    async fn count_in_flight_runs(&self, project: Option<&str>) -> Result<u32, DbError> {
        let row = sqlx::query(
            "SELECT count(*) AS n FROM runs
             WHERE status IN ('running', 'suspended')
               AND ($1::text IS NULL OR project = $1)",
        )
        .bind(project)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row.get::<i64, _>("n") as u32)
    }

    async fn count_running_runs(&self, project: &str) -> Result<u32, DbError> {
        let row =
            sqlx::query("SELECT count(*) AS n FROM runs WHERE status = 'running' AND project = $1")
                .bind(project)
                .fetch_one(self.pool())
                .await
                .map_err(db_err)?;
        Ok(row.get::<i64, _>("n") as u32)
    }

    async fn set_step_gate(
        &self,
        run: &RunId,
        step: &StepId,
        kind: &str,
        timer_seconds: Option<i64>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE step_runs SET gate_kind = $3, gate_timer_seconds = $4,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(kind)
        .bind(timer_seconds)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn gate_timer_seconds(&self, run: &RunId, step: &StepId) -> Result<Option<i64>, DbError> {
        let row = sqlx::query(
            "SELECT gate_timer_seconds FROM step_runs WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<i64>, _>("gate_timer_seconds")))
    }

    async fn set_gate_releaser(
        &self,
        run: &RunId,
        step: &StepId,
        releaser: &str,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE step_runs SET gate_releaser = $3,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(releaser)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn gate_releaser(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        let row =
            sqlx::query("SELECT gate_releaser FROM step_runs WHERE run_id = $1 AND step_id = $2")
                .bind(&run.0)
                .bind(&step.0)
                .fetch_optional(self.pool())
                .await
                .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("gate_releaser")))
    }

    async fn set_gate_wait_inputs(
        &self,
        leader: &LeaderTerm,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        inputs: &Value,
    ) -> Result<bool, DbError> {
        let mut tx = self.begin_fenced(leader).await?;
        let stored = sqlx::query(&format!(
            "UPDATE attempts SET gate_inputs = $4
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3
               AND gate_inputs IS NULL AND {ATTEMPT_IS_RUNNING}"
        ))
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .bind(inputs)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        tx.commit().await.map_err(db_err)?;
        Ok(stored == 1)
    }

    async fn gate_wait_inputs(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<Value>, DbError> {
        let row = sqlx::query(
            "SELECT gate_inputs FROM attempts
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<Value>, _>("gate_inputs")))
    }

    async fn pending_plugin_gates(
        &self,
        org: &str,
        releaser: &str,
        after: Option<&scarab_engine::PendingGateKey>,
        limit: u32,
    ) -> Result<Vec<scarab_engine::PendingGateWait>, DbError> {
        // The open wait is the step's last Attempt while it is running; a
        // gate keeps one open wait at a time, so `running` + inputs stored
        // is that wait.
        let (a_run, a_step, a_attempt) = match after {
            Some(k) => (k.run.as_str(), k.step.as_str(), k.attempt.as_str()),
            None => ("", "", ""),
        };
        let rows = sqlx::query(&format!(
            "SELECT a.run_id, a.step_id, a.attempt_id, a.started_at, a.gate_inputs
             FROM step_runs s
             JOIN runs r ON r.id = s.run_id
             JOIN attempts a ON a.run_id = s.run_id AND a.step_id = s.step_id
             WHERE r.tenant_org = $1
               AND s.gate_releaser = $2
               AND s.gate_kind = 'plugin'
               AND s.status = 'pending'
               AND a.gate_inputs IS NOT NULL
               AND {running}
               AND (a.run_id, a.step_id, a.attempt_id) > ($3, $4, $5)
             ORDER BY a.run_id, a.step_id, a.attempt_id
             LIMIT $6",
            running = ATTEMPT_IS_RUNNING
                .replace("outcome", "a.outcome")
                .replace("failure", "a.failure"),
        ))
        .bind(org)
        .bind(releaser)
        .bind(a_run)
        .bind(a_step)
        .bind(a_attempt)
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| scarab_engine::PendingGateWait {
                run: RunId(r.get("run_id")),
                step: StepId(r.get("step_id")),
                attempt: AttemptId(r.get("attempt_id")),
                started_at: Timestamp(r.get::<i64, _>("started_at")),
                inputs: r.get::<Value, _>("gate_inputs"),
            })
            .collect())
    }

    async fn create_run_service(
        &self,
        leader: Option<&LeaderTerm>,
        run: &RunId,
        take: i64,
        name: &str,
        at: Timestamp,
    ) -> Result<(), DbError> {
        // Idempotent on {run, take, name} (ADR-0058): a re-tick / crash resume
        // never provisions a second instance.
        let mut tx = self.begin_maybe_fenced(leader).await?;
        sqlx::query(
            "INSERT INTO run_services (run_id, take, name, status, created_at, updated_at)
             VALUES ($1, $2, $3, 'starting', $4, $4)
             ON CONFLICT (run_id, take, name) DO NOTHING",
        )
        .bind(&run.0)
        .bind(take)
        .bind(name)
        .bind(at.0)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)
    }

    async fn set_run_service(
        &self,
        leader: Option<&LeaderTerm>,
        run: &RunId,
        take: i64,
        name: &str,
        status: ServiceStatus,
        handle: Option<&str>,
    ) -> Result<(), DbError> {
        // COALESCE keeps a previously-recorded handle when this update omits one.
        let mut tx = self.begin_maybe_fenced(leader).await?;
        sqlx::query(
            "UPDATE run_services
                SET status = $4, handle = COALESCE($5, handle),
                    updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND take = $2 AND name = $3",
        )
        .bind(&run.0)
        .bind(take)
        .bind(name)
        .bind(status.as_str())
        .bind(handle)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)
    }

    async fn run_services(&self, run: &RunId) -> Result<Vec<RunService>, DbError> {
        let rows = sqlx::query(
            "SELECT run_id, take, name, status, handle, created_at
             FROM run_services WHERE run_id = $1 ORDER BY name, take",
        )
        .bind(&run.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.into_iter()
            .map(|r| {
                let status = r.get::<String, _>("status");
                Ok(RunService {
                    run: RunId(r.get::<String, _>("run_id")),
                    take: r.get::<i64, _>("take"),
                    name: r.get::<String, _>("name"),
                    status: ServiceStatus::from_str(&status).ok_or_else(|| {
                        DbError::Other(format!("unknown run_service status `{status}`"))
                    })?,
                    handle: r.get::<Option<String>, _>("handle"),
                    created_at: Timestamp(r.get::<i64, _>("created_at")),
                })
            })
            .collect()
    }

    async fn run_status(&self, run: &RunId) -> Result<Option<RunStatus>, DbError> {
        let row = sqlx::query("SELECT status FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        row.map(|r| run_status_from_str(r.get::<String, _>("status")))
            .transpose()
    }

    async fn run_reopen_generation(&self, run: &RunId) -> Result<u64, DbError> {
        let row = sqlx::query("SELECT reopen_generation FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        Ok(row.map_or(0, |r| r.get::<i64, _>("reopen_generation") as u64))
    }

    async fn active_runs(&self) -> Result<Vec<RunId>, DbError> {
        // Priority order (higher first, then oldest) so the admission pass hands
        // scarce capacity to higher-priority work before lower (ADR-0011, 0032).
        let rows = sqlx::query(
            "SELECT id FROM runs WHERE status IN ('pending', 'running', 'suspended')
               AND NOT creating
             ORDER BY priority DESC, created_at",
        )
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| RunId(r.get::<String, _>("id")))
            .collect())
    }

    async fn list_runs(&self, limit: u32) -> Result<Vec<RunSummary>, DbError> {
        let rows = sqlx::query(
            "SELECT id, status, created_at, updated_at, tenant_org, tenant_project, run_number,
                    origin_trigger_kind, origin_actor, origin_ref, origin_sha, origin_pr_number,
                    origin_pr_base, pipeline, trigger_title, deploy_environment, is_deploy_run
             FROM runs
             ORDER BY created_at DESC, id DESC
             LIMIT $1",
        )
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.into_iter().map(run_summary_from_row).collect()
    }

    async fn list_runs_for_org(&self, org: &str, limit: u32) -> Result<Vec<RunSummary>, DbError> {
        // `runs_tenant_idx` leads with `tenant_org`.
        let rows = sqlx::query(
            "SELECT id, status, created_at, updated_at, tenant_org, tenant_project, run_number,
                    origin_trigger_kind, origin_actor, origin_ref, origin_sha, origin_pr_number,
                    origin_pr_base, pipeline, trigger_title, deploy_environment, is_deploy_run
             FROM runs
             WHERE tenant_org = $1
             ORDER BY created_at DESC, id DESC
             LIMIT $2",
        )
        .bind(org)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.into_iter().map(run_summary_from_row).collect()
    }

    async fn list_runs_for_tenant(
        &self,
        org: &str,
        project: &str,
        filter: &scarab_engine::RunListFilter,
        limit: u32,
    ) -> Result<Vec<RunSummary>, DbError> {
        // A NULL parameter disengages its filter. Both apply in the WHERE,
        // before the LIMIT, so a filtered page is the newest matches.
        let rows = sqlx::query(
            "SELECT id, status, created_at, updated_at, tenant_org, tenant_project, run_number,
                    origin_trigger_kind, origin_actor, origin_ref, origin_sha, origin_pr_number,
                    origin_pr_base, pipeline, trigger_title, deploy_environment, is_deploy_run
             FROM runs
             WHERE tenant_org = $1 AND tenant_project = $2
               AND ($3::text IS NULL OR deploy_environment = $3)
               AND ($4::boolean IS NULL OR is_deploy_run = $4)
             ORDER BY created_at DESC, id DESC
             LIMIT $5",
        )
        .bind(org)
        .bind(project)
        .bind(filter.environment.as_deref())
        .bind(filter.deploy)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.into_iter().map(run_summary_from_row).collect()
    }

    async fn projects_with_runs_in(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<String>, DbError> {
        sqlx::query_scalar(
            "SELECT deploy_project FROM runs
             WHERE deploy_org = $1 AND deploy_environment = $2 AND deploy_project IS NOT NULL
             GROUP BY deploy_project
             ORDER BY deploy_project COLLATE \"C\"",
        )
        .bind(org)
        .bind(environment)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)
    }

    async fn record_refused_run(&self, r: &scarab_engine::RefusedRun) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO refused_runs (org, project, pipeline, trigger_kind, actor, git_ref, sha,
                                       title, reason_kind, message, errors, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(&r.org)
        .bind(&r.project)
        .bind(&r.pipeline)
        .bind(&r.trigger_kind)
        .bind(r.actor.as_deref())
        .bind(r.git_ref.as_deref())
        .bind(r.sha.as_deref())
        .bind(r.title.as_deref())
        .bind(&r.reason_kind)
        .bind(&r.message)
        .bind(&r.errors)
        .bind(r.created_at.0)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn refused_runs_for_tenant(
        &self,
        org: &str,
        project: &str,
        limit: u32,
    ) -> Result<Vec<scarab_engine::RefusedRun>, DbError> {
        let rows = sqlx::query(
            "SELECT id, org, project, pipeline, trigger_kind, actor, git_ref, sha, title,
                    reason_kind, message, errors, created_at
             FROM refused_runs
             WHERE org = $1 AND project = $2
             ORDER BY created_at DESC, id DESC
             LIMIT $3",
        )
        .bind(org)
        .bind(project)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| scarab_engine::RefusedRun {
                id: r.get("id"),
                org: r.get("org"),
                project: r.get("project"),
                pipeline: r.get("pipeline"),
                trigger_kind: r.get("trigger_kind"),
                actor: r.get("actor"),
                git_ref: r.get("git_ref"),
                sha: r.get("sha"),
                title: r.get("title"),
                reason_kind: r.get("reason_kind"),
                message: r.get("message"),
                errors: r.get("errors"),
                created_at: Timestamp(r.get("created_at")),
            })
            .collect())
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventKind>, DbError> {
        let rows =
            sqlx::query("SELECT version, at, payload FROM events WHERE run_id = $1 ORDER BY seq")
                .bind(&run.0)
                .fetch_all(self.pool())
                .await
                .map_err(db_err)?;
        rows.into_iter()
            .map(|r| {
                let payload: Value = r.get("payload");
                let kind: EventPayload =
                    serde_json::from_value(payload).map_err(|e| DbError::Other(e.to_string()))?;
                Ok(EventKind {
                    version: r.get::<i32, _>("version") as u32,
                    run: run.clone(),
                    kind,
                    at: Timestamp(r.get::<i64, _>("at")),
                })
            })
            .collect()
    }

    async fn steps_of_run(&self, run: &RunId) -> Result<Vec<StepRun>, DbError> {
        let rows = sqlx::query(
            "SELECT step_id, status, needs, gate_kind FROM step_runs WHERE run_id = $1 ORDER BY step_id",
        )
        .bind(&run.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let mut steps = Vec::with_capacity(rows.len());
        for r in rows {
            let step = StepId(r.get::<String, _>("step_id"));
            let status = step_status_from_str(r.get::<String, _>("status"))?;
            let needs = needs_from_value(r.get::<Value, _>("needs"))?;
            let gate_kind = r.get::<Option<String>, _>("gate_kind");
            let attempts = self.attempts(run, &step).await?;
            steps.push(StepRun {
                run: run.clone(),
                step,
                status,
                attempts,
                needs,
                gate_kind,
            });
        }
        Ok(steps)
    }

    async fn steps_of_runs(
        &self,
        runs: &[RunId],
    ) -> Result<std::collections::HashMap<RunId, Vec<StepRun>>, DbError> {
        let mut out: std::collections::HashMap<RunId, Vec<StepRun>> =
            std::collections::HashMap::new();
        if runs.is_empty() {
            return Ok(out);
        }
        let ids: Vec<&str> = runs.iter().map(|r| r.0.as_str()).collect();
        // Two queries for the whole page, in `steps_of_run`'s orders: steps by
        // id, each step's attempts by their `a{n}` number.
        let rows = sqlx::query(
            "SELECT run_id, step_id, status, needs, gate_kind FROM step_runs
             WHERE run_id = ANY($1) ORDER BY run_id, step_id",
        )
        .bind(&ids)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let attempt_rows = sqlx::query(
            "SELECT run_id, step_id, attempt_id, started_at, failure, failure_detail, outcome
             FROM attempts
             WHERE run_id = ANY($1)
             ORDER BY run_id, step_id, CAST(substring(attempt_id FROM 2) AS INTEGER)",
        )
        .bind(&ids)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let mut attempts: std::collections::HashMap<(String, String), Vec<Attempt>> =
            std::collections::HashMap::new();
        for r in &attempt_rows {
            attempts
                .entry((r.get::<String, _>("run_id"), r.get::<String, _>("step_id")))
                .or_default()
                .push(attempt_from_row(r)?);
        }
        for r in rows {
            let run = r.get::<String, _>("run_id");
            let step = r.get::<String, _>("step_id");
            let status = step_status_from_str(r.get::<String, _>("status"))?;
            let needs = needs_from_value(r.get::<Value, _>("needs"))?;
            let gate_kind = r.get::<Option<String>, _>("gate_kind");
            let step_attempts = attempts
                .remove(&(run.clone(), step.clone()))
                .unwrap_or_default();
            out.entry(RunId(run.clone())).or_default().push(StepRun {
                run: RunId(run),
                step: StepId(step),
                status,
                attempts: step_attempts,
                needs,
                gate_kind,
            });
        }
        Ok(out)
    }

    async fn step_spec(&self, run: &RunId, step: &StepId) -> Result<Option<StepSpec>, DbError> {
        let row = sqlx::query("SELECT spec FROM step_runs WHERE run_id = $1 AND step_id = $2")
            .bind(&run.0)
            .bind(&step.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        match row.and_then(|r| r.get::<Option<Value>, _>("spec")) {
            Some(v) => Ok(Some(
                serde_json::from_value(v).map_err(|e| DbError::Other(e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    async fn append_log_chunk(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        meta: &LogChunkMeta,
    ) -> Result<(), DbError> {
        // Offsets only — the body is in the object store. Idempotent on the seq.
        sqlx::query(
            "INSERT INTO log_chunks
               (run_id, step_id, attempt_id, seq, byte_offset, len, object_key, at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, (extract(epoch from now()) * 1000)::bigint)
             ON CONFLICT (run_id, step_id, attempt_id, seq) DO NOTHING",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .bind(meta.seq as i64)
        .bind(meta.byte_offset as i64)
        .bind(meta.len as i64)
        .bind(&meta.object_key)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn log_chunks(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Vec<LogChunkMeta>, DbError> {
        let rows = sqlx::query(
            "SELECT seq, byte_offset, len, object_key FROM log_chunks
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3 ORDER BY seq",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| LogChunkMeta {
                seq: r.get::<i64, _>("seq") as u64,
                byte_offset: r.get::<i64, _>("byte_offset") as u64,
                len: r.get::<i64, _>("len") as u64,
                object_key: r.get::<String, _>("object_key"),
            })
            .collect())
    }

    async fn record_step_transition(
        &self,
        run: &RunId,
        step: &StepId,
        from: StepStatus,
        to: StepStatus,
    ) -> Result<(), DbError> {
        // Optimistic guard, like record_transition: the UPDATE fires only if the
        // step is still in `from`, so a stale/duplicate finalize is a Conflict.
        if !cas_step_status(self.pool(), run, step, from, to).await? {
            return Err(DbError::Conflict);
        }
        Ok(())
    }

    async fn commit_step_transition(
        &self,
        run: &RunId,
        step: &StepId,
        from: StepStatus,
        to: StepStatus,
        frontier: scarab_engine::AttemptFrontier<'_>,
        stamp: Option<(&AttemptId, &scarab_engine::AttemptStamp)>,
        events: &[EventKind],
        outbox: &[OutboxMessage],
    ) -> Result<(), DbError> {
        // ONE transaction (git-bug 586187a): the CAS takes the step row's
        // lock first, so a peer's competing CAS waits and then sees the
        // committed status (Conflict) — or, if we roll back, the old one. An
        // early return drops `tx`, which rolls everything back.
        let mut tx = self.pool().begin().await.map_err(db_err)?;
        if !cas_step_status(&mut *tx, run, step, from, to).await? {
            tx.rollback().await.map_err(db_err)?;
            return Err(DbError::Conflict);
        }
        // The frontier (red team R2-E3), read only now that the CAS holds
        // the step row: `record_attempt` holds it `FOR SHARE` while it mints,
        // so an attempt minted concurrently is either visible to this
        // statement (READ COMMITTED: a fresh snapshot) or minted after this
        // commit, under the status it writes.
        if let scarab_engine::AttemptFrontier::Is(expected) = frontier {
            let latest: Option<String> = sqlx::query_scalar(
                "SELECT attempt_id FROM attempts
                 WHERE run_id = $1 AND step_id = $2
                 ORDER BY CAST(substring(attempt_id FROM 2) AS INTEGER) DESC
                 LIMIT 1",
            )
            .bind(&run.0)
            .bind(&step.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            if latest.as_deref() != expected.map(|a| a.0.as_str()) {
                tx.rollback().await.map_err(db_err)?;
                return Err(DbError::Conflict);
            }
        }
        match stamp {
            Some((attempt, scarab_engine::AttemptStamp::Outcome(outcome))) => {
                update_attempt_outcome(&mut *tx, run, step, attempt, *outcome).await?;
            }
            Some((attempt, scarab_engine::AttemptStamp::Failure { kind, detail })) => {
                update_attempt_failure(&mut *tx, run, step, attempt, *kind, detail.as_deref())
                    .await?;
            }
            None => {}
        }
        for event in events {
            insert_event(&mut *tx, event).await?;
        }
        for msg in outbox {
            insert_outbox(&mut *tx, msg).await?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn record_attempt(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &Attempt,
    ) -> Result<(), DbError> {
        // Idempotent AND non-downgrading on the monotonic attempt id (the
        // fencing unit). `record_attempt` always mints a FRESH row
        // (failure=NULL, outcome='running') at launch/adoption; a re-drive
        // (crash re-adoption, idempotent re-launch) must therefore NEVER
        // overwrite evidence a later `set_attempt_failure`/`set_attempt_outcome`
        // already recorded on this id — a `DO UPDATE` would reset a real
        // Failed/Superseded/Cancelled verdict (and its failure classification)
        // back to running/NULL: silent evidence loss. The row already holds the
        // authoritative evidence and nothing in this INSERT legitimately
        // refreshes (the launch handle is a separate column, written by
        // `set_attempt_handle`), so keep the existing row untouched.
        //
        // The mint holds the step row `FOR SHARE` (see the port): a step
        // transition's frontier check either sees this attempt or commits
        // first, so the attempt is minted under the status it wrote.
        let mut tx = self.pool().begin().await.map_err(db_err)?;
        sqlx::query("SELECT 1 FROM step_runs WHERE run_id = $1 AND step_id = $2 FOR SHARE")
            .bind(&run.0)
            .bind(&step.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query(
            "INSERT INTO attempts
                 (run_id, step_id, attempt_id, started_at, failure, failure_detail, outcome)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (run_id, step_id, attempt_id) DO NOTHING",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.id.0)
        .bind(attempt.started_at.0)
        .bind(attempt.failure.map(failure_str))
        .bind(attempt.failure_detail.as_deref())
        .bind(attempt.outcome.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)
    }

    async fn attempts_of_step(&self, run: &RunId, step: &StepId) -> Result<Vec<Attempt>, DbError> {
        self.attempts(run, step).await
    }

    async fn set_attempt_handle(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        handle: &str,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE attempts SET handle = $4
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .bind(handle)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn attempt_handle(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<String>, DbError> {
        let row = sqlx::query(
            "SELECT handle FROM attempts
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("handle")))
    }

    async fn mark_attempt_execution_started(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        at: Timestamp,
    ) -> Result<Timestamp, DbError> {
        // Writes only while the column is NULL, so the poll of a Running
        // attempt that is already stamped writes no new row version.
        let row = sqlx::query(
            "WITH stamped AS (
                 UPDATE attempts SET execution_started_at = $4
                 WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3
                   AND execution_started_at IS NULL
                 RETURNING execution_started_at
             )
             SELECT execution_started_at FROM stamped
             UNION ALL
             SELECT execution_started_at FROM attempts
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3
               AND NOT EXISTS (SELECT 1 FROM stamped)",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .bind(at.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row
            .and_then(|r| r.get::<Option<i64>, _>("execution_started_at"))
            .map(Timestamp)
            .unwrap_or(at))
    }

    async fn attempt_execution_started(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<Timestamp>, DbError> {
        let row = sqlx::query(
            "SELECT execution_started_at FROM attempts
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row
            .and_then(|r| r.get::<Option<i64>, _>("execution_started_at"))
            .map(Timestamp))
    }

    async fn set_attempt_failure(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        failure: FailureKind,
        detail: Option<&str>,
    ) -> Result<(), DbError> {
        // Record the classification, the human-readable cause (4cf03d7) and the
        // `Failed` outcome together so the columns never diverge (ADR-0056
        // amendment). Defense in depth: never
        // downgrade a terminal-by-intent outcome — a rerun (`superseded`) or a
        // run cancel (`cancelled`) tore this attempt down on purpose, and the
        // self-inflicted `Lost` its dying Pod reports must not clobber that
        // verdict. Two `IS DISTINCT FROM` clauses (not `NOT IN`) keep NULL/other
        // outcomes writable — `outcome` is nullable and `NULL NOT IN (…)` is NULL,
        // which would wrongly refuse the legitimate write to a pre-outcome row.
        update_attempt_failure(self.pool(), run, step, attempt, failure, detail).await
    }

    async fn set_attempt_outcome(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        outcome: AttemptOutcome,
    ) -> Result<(), DbError> {
        // Defense in depth (mirrors `set_attempt_failure`): a terminal-by-intent
        // outcome (`superseded` from a rerun, `cancelled` from a run cancel) is
        // never overwritten — the intentional teardown verdict wins over any later
        // observation. Setting `Superseded`/`Cancelled` itself is still fine (the
        // row is `running` at that point). `IS DISTINCT FROM` (not `NOT IN`) keeps
        // the nullable pre-outcome rows writable.
        update_attempt_outcome(self.pool(), run, step, attempt, outcome).await
    }

    async fn record_transition(
        &self,
        run: &RunId,
        from: RunStatus,
        to: RunStatus,
    ) -> Result<(), DbError> {
        // Optimistic concurrency: the UPDATE only fires if the row is still in
        // `from`. Zero rows affected means a concurrent/duplicate writer already
        // moved it (e.g. a crashed worker re-driving) → Conflict, not a second
        // advance. This is the state-table guard behind exactly-once admission.
        if !cas_run_status(self.pool(), run, from, to, None).await? {
            return Err(DbError::Conflict);
        }
        Ok(())
    }

    async fn commit_run_transition(
        &self,
        leader: Option<&LeaderTerm>,
        run: &RunId,
        from: RunStatus,
        to: RunStatus,
        generation: u64,
        events: &[EventKind],
        outbox: &[OutboxMessage],
    ) -> Result<(), DbError> {
        // ONE transaction: the CAS takes the run row's lock first, so a
        // competing CAS waits and then sees the committed status (Conflict).
        // An early return drops `tx`, which rolls everything back. A leader's
        // transition is fenced on its term in this same transaction.
        let mut tx = self.begin_maybe_fenced(leader).await?;
        if !cas_run_status(&mut *tx, run, from, to, Some(generation)).await? {
            tx.rollback().await.map_err(db_err)?;
            return Err(DbError::Conflict);
        }
        for event in events {
            insert_event(&mut *tx, event).await?;
        }
        for msg in outbox {
            insert_outbox(&mut *tx, msg).await?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn append_event(&self, event: &EventKind) -> Result<(), DbError> {
        insert_event(self.pool(), event).await
    }

    async fn open_gate_wait(
        &self,
        leader: &LeaderTerm,
        run: &RunId,
        step: &StepId,
        started_at: Timestamp,
        at: Timestamp,
    ) -> Result<Option<Attempt>, DbError> {
        // One transaction with the step row locked: a concurrent resolve or
        // cancel of this gate waits for the mint, or the mint sees its result.
        // An early return drops `tx`, which rolls it back.
        let mut tx = self.begin_fenced(leader).await?;
        if !lock_pending_step(&mut tx, run, step).await? {
            return Ok(None);
        }
        let attempts = attempts_on(&mut *tx, run, step).await?;
        if let Some(open) = attempts
            .last()
            .filter(|a| a.outcome == AttemptOutcome::Running)
            .cloned()
        {
            tx.commit().await.map_err(db_err)?;
            return Ok(Some(open));
        }
        let attempt = Attempt {
            id: AttemptId(format!("a{}", next_attempt_number(&attempts))),
            started_at: Timestamp(started_at.0.min(at.0)),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        };
        insert_attempt(&mut tx, run, step, &attempt).await?;
        insert_event(
            &mut *tx,
            &EventKind {
                version: scarab_engine::EVENT_VERSION,
                run: run.clone(),
                kind: EventPayload::AttemptStarted {
                    step: step.clone(),
                    attempt: attempt.id.clone(),
                },
                at,
            },
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        Ok(Some(attempt))
    }

    async fn resolve_gate_wait(
        &self,
        run: &RunId,
        step: &StepId,
        res: &scarab_engine::GateResolution,
    ) -> Result<Option<AttemptId>, DbError> {
        // All of it or none of it: the Attempt closed, the step CAS and the
        // journal commit together. Every early return rolls back.
        let mut tx = self.pool().begin().await.map_err(db_err)?;
        if !lock_pending_step(&mut tx, run, step).await? {
            return Ok(None);
        }
        let (failure, outcome, to) = if res.passed {
            (None, AttemptOutcome::Succeeded, StepStatus::Succeeded)
        } else {
            (
                Some(FailureKind::Gate),
                AttemptOutcome::Failed,
                StepStatus::Failed,
            )
        };
        let detail = res.attempt_detail();
        let open = match &res.fence {
            Some(f) => Some(f.clone()),
            None => attempts_on(&mut *tx, run, step)
                .await?
                .last()
                .filter(|a| a.outcome == AttemptOutcome::Running)
                .map(|a| a.id.clone()),
        };
        let (attempt, minted) = match open {
            // The fenced (or open) wait closes only while it is still running:
            // a wait superseded or cancelled since the caller's snapshot takes
            // no verdict.
            Some(a) => {
                let closed =
                    close_attempt_on(&mut tx, run, step, &a, outcome, failure, detail.as_deref())
                        .await?;
                if !closed {
                    return Ok(None);
                }
                (a, false)
            }
            // Unfenced, no open wait (a pre-upgrade suspended run): mint the
            // Attempt already closed.
            None => {
                let attempts = attempts_on(&mut *tx, run, step).await?;
                let minted = Attempt {
                    id: AttemptId(format!("a{}", next_attempt_number(&attempts))),
                    started_at: Timestamp(res.legacy_started_at.0.min(res.at.0)),
                    failure,
                    failure_detail: detail.clone(),
                    outcome,
                };
                insert_attempt(&mut tx, run, step, &minted).await?;
                (minted.id, true)
            }
        };
        let moved = sqlx::query(
            "UPDATE step_runs
             SET status = $4, updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND step_id = $2 AND status = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(step_status_str(StepStatus::Pending))
        .bind(step_status_str(to))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if moved == 0 {
            return Ok(None);
        }
        // A release's outputs (ADR-0076 §7) are the gate's results, written
        // with the verdict so a dependent never runs without them.
        if let (true, Some(outputs)) = (res.passed, &res.outputs) {
            let json = serde_json::to_value(outputs).map_err(|e| DbError::Other(e.to_string()))?;
            sqlx::query(
                "UPDATE step_runs SET results = $3, evidence_attempt = $4
                 WHERE run_id = $1 AND step_id = $2",
            )
            .bind(&run.0)
            .bind(&step.0)
            .bind(&json)
            .bind(&attempt.0)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            sqlx::query(
                "UPDATE attempts SET results = $4
                 WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
            )
            .bind(&run.0)
            .bind(&step.0)
            .bind(&attempt.0)
            .bind(&json)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        for kind in scarab_engine::gate_resolution_events(step, &attempt, minted, res) {
            insert_event(
                &mut *tx,
                &EventKind {
                    version: scarab_engine::EVENT_VERSION,
                    run: run.clone(),
                    kind,
                    at: res.at,
                },
            )
            .await?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(Some(attempt))
    }

    async fn close_running_attempt(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        outcome: AttemptOutcome,
        event: Option<&EventKind>,
    ) -> Result<bool, DbError> {
        let mut tx = self.pool().begin().await.map_err(db_err)?;
        let closed = sqlx::query(&format!(
            "UPDATE attempts SET outcome = $4
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3 AND {ATTEMPT_IS_RUNNING}"
        ))
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .bind(outcome.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if closed == 0 {
            return Ok(false);
        }
        if let Some(e) = event {
            insert_event(&mut *tx, e).await?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(true)
    }

    async fn enqueue_outbox(&self, msg: &OutboxMessage) -> Result<(), DbError> {
        insert_outbox(self.pool(), msg).await
    }

    async fn claim_outbox(
        &self,
        owner: &str,
        kind: Option<&str>,
        limit: u32,
        visibility_ms: i64,
    ) -> Result<Vec<OutboxMessage>, DbError> {
        // Hand out undispatched rows whose claim (if any) has expired, hiding
        // them for `visibility_ms`. SKIP LOCKED keeps concurrent drainers on
        // disjoint sets; the visibility timeout makes a crashed drainer's rows
        // reclaimable rather than lost. `kind` (when set) scopes the claim so
        // distinct drainers don't contend over each other's messages.
        let rows = sqlx::query(
            "UPDATE outbox
             SET claimed_by = $1,
                 claimed_until = (extract(epoch from now()) * 1000)::bigint + $3
             WHERE id IN (
                 SELECT id FROM outbox
                 WHERE dispatched_at IS NULL
                   AND dead_lettered_at IS NULL
                   AND ($4::text IS NULL OR kind = $4)
                   AND (claimed_until IS NULL
                        OR claimed_until <= (extract(epoch from now()) * 1000)::bigint)
                 ORDER BY id
                 FOR UPDATE SKIP LOCKED
                 LIMIT $2
             )
             RETURNING id, run_id, kind, payload, idempotency_key, created_at",
        )
        .bind(owner)
        .bind(limit as i64)
        .bind(visibility_ms)
        .bind(kind)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;

        Ok(rows
            .into_iter()
            .map(|r| OutboxMessage {
                id: OutboxId(r.get::<i64, _>("id")),
                run: RunId(r.get::<String, _>("run_id")),
                kind: r.get::<String, _>("kind"),
                payload: r.get::<Value, _>("payload"),
                idempotency_key: r.get::<String, _>("idempotency_key"),
                at: Timestamp(r.get::<i64, _>("created_at")),
            })
            .collect())
    }

    async fn renew_outbox_claim(
        &self,
        id: OutboxId,
        owner: &str,
        visibility_ms: i64,
    ) -> Result<bool, DbError> {
        // Conditional on `claimed_by`: a claim that lapsed and was taken by
        // another drainer is theirs now, and this renewal must not steal it
        // back (git-bug 068cc0b). A lapsed claim nobody re-took is still ours,
        // and extending it is harmless — nobody else is driving the message.
        let renewed = sqlx::query(
            "UPDATE outbox
             SET claimed_until = (extract(epoch from now()) * 1000)::bigint + $3
             WHERE id = $1 AND claimed_by = $2
               AND dispatched_at IS NULL AND dead_lettered_at IS NULL",
        )
        .bind(id.0)
        .bind(owner)
        .bind(visibility_ms)
        .execute(self.pool())
        .await
        .map_err(db_err)?
        .rows_affected();
        Ok(renewed == 1)
    }

    async fn mark_dispatched(&self, id: OutboxId) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE outbox SET dispatched_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(id.0)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn outbox_key_pending(&self, idempotency_key: &str) -> Result<bool, DbError> {
        let pending: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM outbox
                 WHERE idempotency_key = $1
                   AND dispatched_at IS NULL AND dead_lettered_at IS NULL)",
        )
        .bind(idempotency_key)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        Ok(pending)
    }

    async fn record_outbox_failure(&self, id: OutboxId) -> Result<u32, DbError> {
        let row = sqlx::query(
            "UPDATE outbox SET delivery_attempts = delivery_attempts + 1
             WHERE id = $1
             RETURNING delivery_attempts",
        )
        .bind(id.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row
            .map(|r| r.get::<i64, _>("delivery_attempts") as u32)
            .unwrap_or(0))
    }

    async fn dead_letter_outbox(&self, id: OutboxId) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE outbox SET dead_lettered_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(id.0)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_status_counts(&self) -> Result<Vec<(String, u64)>, DbError> {
        let rows = sqlx::query("SELECT status, count(*) AS n FROM runs GROUP BY status")
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("status"), r.get::<i64, _>("n") as u64))
            .collect())
    }

    async fn outbox_depth(&self) -> Result<u64, DbError> {
        let row = sqlx::query(
            "SELECT count(*) AS n FROM outbox
             WHERE dispatched_at IS NULL AND dead_lettered_at IS NULL",
        )
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        Ok(row.get::<i64, _>("n") as u64)
    }

    async fn put_artifacts(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        succeeded: bool,
        artifacts: &[scarab_engine::ArtifactMeta],
        at: Timestamp,
    ) -> Result<(), DbError> {
        for a in artifacts {
            // Immutable per attempt (ADR-0056): the conflict target includes
            // the attempt, so only a re-drive of the SAME fenced attempt
            // overwrites (deterministic — same bytes); a new attempt writes a
            // new version and prior evidence survives.
            sqlx::query(
                "INSERT INTO artifacts
                     (run_id, name, step_id, attempt_id, succeeded, size, content_type,
                      object_key, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT (run_id, name, step_id, attempt_id) DO UPDATE SET
                     succeeded = EXCLUDED.succeeded,
                     size = EXCLUDED.size,
                     content_type = EXCLUDED.content_type,
                     object_key = EXCLUDED.object_key",
            )
            .bind(&run.0)
            .bind(&a.name)
            .bind(&step.0)
            .bind(&attempt.0)
            .bind(succeeded)
            .bind(a.size as i64)
            .bind(&a.content_type)
            .bind(&a.object_key)
            .bind(at.0)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        }
        Ok(())
    }

    async fn artifacts_of_run(
        &self,
        run: &RunId,
    ) -> Result<Vec<scarab_engine::ArtifactRecord>, DbError> {
        let rows = sqlx::query(
            "SELECT name, step_id, attempt_id, succeeded, size, content_type, object_key,
                    created_at
             FROM artifacts
             WHERE run_id = $1 ORDER BY name, created_at, attempt_id",
        )
        .bind(&run.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| scarab_engine::ArtifactRecord {
                meta: scarab_engine::ArtifactMeta {
                    name: r.get::<String, _>("name"),
                    size: r.get::<i64, _>("size") as u64,
                    content_type: r.get::<String, _>("content_type"),
                    object_key: r.get::<String, _>("object_key"),
                },
                step: StepId(r.get::<String, _>("step_id")),
                attempt: AttemptId(r.get::<String, _>("attempt_id")),
                succeeded: r.get::<bool, _>("succeeded"),
                created_at: Timestamp(r.get::<i64, _>("created_at")),
            })
            .collect())
    }

    async fn prunable_artifact_runs(
        &self,
        cutoff: Timestamp,
        limit: u32,
    ) -> Result<Vec<RunId>, DbError> {
        let rows = sqlx::query(
            "SELECT DISTINCT r.id FROM runs r
             JOIN artifacts a ON a.run_id = r.id
             WHERE r.status IN ('succeeded', 'failed', 'cancelled', 'dead_lettered')
               AND r.updated_at < $1
             LIMIT $2",
        )
        .bind(cutoff.0)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| RunId(r.get::<String, _>("id")))
            .collect())
    }

    async fn delete_artifacts_of_run(&self, run: &RunId) -> Result<(), DbError> {
        sqlx::query("DELETE FROM artifacts WHERE run_id = $1")
            .bind(&run.0)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn prunable_log_runs(
        &self,
        cutoff: Timestamp,
        limit: u32,
    ) -> Result<Vec<RunId>, DbError> {
        let rows = sqlx::query(
            "SELECT DISTINCT r.id FROM runs r
             JOIN log_chunks lc ON lc.run_id = r.id
             WHERE r.status IN ('succeeded', 'failed', 'cancelled', 'dead_lettered')
               AND r.updated_at < $1
             LIMIT $2",
        )
        .bind(cutoff.0)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| RunId(r.get::<String, _>("id")))
            .collect())
    }

    async fn log_object_keys_of_run(&self, run: &RunId) -> Result<Vec<String>, DbError> {
        let rows = sqlx::query("SELECT object_key FROM log_chunks WHERE run_id = $1")
            .bind(&run.0)
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("object_key"))
            .collect())
    }

    async fn delete_log_index_of_run(&self, run: &RunId) -> Result<(), DbError> {
        sqlx::query("DELETE FROM log_chunks WHERE run_id = $1")
            .bind(&run.0)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn gc_workspace_roots(
        &self,
        terminal_cutoff: Timestamp,
    ) -> Result<Vec<(String, Timestamp)>, DbError> {
        // EVERY attempt's snapshot is live while its run is (ADR-0056), not
        // just each step's latest — an old Take's workspace view must never
        // race the sweeper. The step_runs arm is kept for pre-ADR-0056 rows
        // whose attempts carry no snapshot copy.
        //
        // `snapshots_pinned_at IS NOT NULL` (ADR-0061 s5) is a third disjunct in
        // the reachability predicate, alongside "non-terminal" and "within TTL".
        // A pin therefore enters the **mark**, so the whole transitive tree under
        // a pinned root survives — including subtrees shared with runs that are
        // themselves collectable. Filtering the delete list instead would keep the
        // root object and sweep the blobs beneath it, i.e. keep a pointer to
        // nothing, which is the one outcome a pin must never produce.
        //
        // Each root travels with its RECORDING clock — when the reference row
        // was written. `step_runs.updated_at` is stamped by `set_step_output`
        // in the same UPDATE that writes the snapshot; attempts carry no write
        // stamp, so their arm uses `started_at`, which can only be EARLIER
        // than the recording. `MIN` per root keeps the earliest: one old
        // recording proves the cold flush (ADR-0064) had time to land, so the
        // sweeper's torn-cold alarm must not be suppressed merely because a
        // younger run re-recorded the same root.
        let rows = sqlx::query(
            "SELECT root, MIN(at) AS recorded_at FROM (
                 SELECT sr.output_snapshot AS root, sr.updated_at AS at FROM step_runs sr
                 JOIN runs r ON r.id = sr.run_id
                 WHERE sr.output_snapshot IS NOT NULL
                   AND (r.status NOT IN ('succeeded', 'failed', 'cancelled', 'dead_lettered')
                        OR r.updated_at >= $1
                        OR r.snapshots_pinned_at IS NOT NULL)
                 UNION ALL
                 SELECT a.output_snapshot AS root, a.started_at AS at FROM attempts a
                 JOIN runs r ON r.id = a.run_id
                 WHERE a.output_snapshot IS NOT NULL
                   AND (r.status NOT IN ('succeeded', 'failed', 'cancelled', 'dead_lettered')
                        OR r.updated_at >= $1
                        OR r.snapshots_pinned_at IS NOT NULL)
             ) refs
             GROUP BY root",
        )
        .bind(terminal_cutoff.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("root"),
                    Timestamp(r.get::<i64, _>("recorded_at")),
                )
            })
            .collect())
    }

    async fn pin_run_snapshots(
        &self,
        run: &RunId,
        by: Option<&str>,
        at: Timestamp,
    ) -> Result<bool, DbError> {
        // Deliberately NOT touching `updated_at`: that column is the run's
        // lifecycle clock and the TTL cutoff `gc_workspace_roots` compares
        // against. A pin must not silently re-date the run, or "pinned" and
        // "settled 5 minutes ago" would become indistinguishable in every view.
        let n = sqlx::query(
            "UPDATE runs SET snapshots_pinned_at = $2, snapshots_pinned_by = $3 WHERE id = $1",
        )
        .bind(&run.0)
        .bind(at.0)
        .bind(by)
        .execute(self.pool())
        .await
        .map_err(db_err)?
        .rows_affected();
        Ok(n > 0)
    }

    async fn unpin_run_snapshots(&self, run: &RunId) -> Result<bool, DbError> {
        let n = sqlx::query(
            "UPDATE runs SET snapshots_pinned_at = NULL, snapshots_pinned_by = NULL WHERE id = $1",
        )
        .bind(&run.0)
        .execute(self.pool())
        .await
        .map_err(db_err)?
        .rows_affected();
        Ok(n > 0)
    }

    async fn run_snapshot_retention(
        &self,
        run: &RunId,
    ) -> Result<Option<scarab_engine::SnapshotRetention>, DbError> {
        let row = sqlx::query(
            "SELECT status, updated_at, snapshots_pinned_at, snapshots_pinned_by
             FROM runs WHERE id = $1",
        )
        .bind(&run.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        let Some(row) = row else { return Ok(None) };
        let status: String = row.get("status");
        Ok(Some(scarab_engine::SnapshotRetention {
            // The same terminal vocabulary the mark query filters on, so "on the
            // TTL clock" means one thing in both places.
            terminal: matches!(
                status.as_str(),
                "succeeded" | "failed" | "cancelled" | "dead_lettered"
            ),
            settled_at: Timestamp(row.get::<i64, _>("updated_at")),
            pinned_at: row
                .get::<Option<i64>, _>("snapshots_pinned_at")
                .map(Timestamp),
            pinned_by: row.get::<Option<String>, _>("snapshots_pinned_by"),
        }))
    }

    async fn forget_workspace_root(&self, root: &str) -> Result<u32, DbError> {
        // BOTH arms of the mark set (ADR-0056): the step_runs denorm and every
        // attempt's own copy. Clearing one arm only would leave the other still
        // reporting the dead root, so the warning would survive the self-heal.
        let mut cleared = 0u64;
        for sql in [
            "UPDATE step_runs SET output_snapshot = NULL WHERE output_snapshot = $1",
            "UPDATE attempts SET output_snapshot = NULL WHERE output_snapshot = $1",
        ] {
            cleared += sqlx::query(sql)
                .bind(root)
                .execute(self.pool())
                .await
                .map_err(db_err)?
                .rows_affected();
        }
        Ok(cleared as u32)
    }

    async fn release_lease(&self, resource: &str, owner: &str) -> Result<(), DbError> {
        // Expire it, keeping owner and term: the next lease() by anyone is
        // a takeover of an expired lease, which starts the next term. The
        // row is guarded on its owner, so a lease already taken over is not
        // touched. Bounded like a takeover, should this owner still be
        // inside one of its own fenced transactions.
        let mut tx = self.pool().begin().await.map_err(db_err)?;
        sqlx::query(&format!("SET LOCAL lock_timeout = '{TAKEOVER_WAIT}'"))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query(
            "UPDATE leases SET expires_at = (extract(epoch from now()) * 1000)::bigint - 1
             WHERE resource = $1 AND owner = $2",
        )
        .bind(resource)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)
    }

    async fn lease(&self, resource: &str, owner: &str, ttl_ms: i64) -> Result<Lease, DbError> {
        // Read first, without a lock. Only the holder's renewal and the
        // takeover of an EXPIRED lease write the row, so a follower that finds
        // the lease live never waits on the holder's fenced transactions
        // (they hold the row FOR SHARE, git-bug aaf4e3e): its tick is not
        // slowed by the leader's writes. Every write below is guarded on the
        // row it read; a lost race re-reads and reports the winner.
        for _ in 0..3 {
            let current = sqlx::query(
                "SELECT owner, expires_at, term,
                        expires_at < (extract(epoch from now()) * 1000)::bigint AS expired
                 FROM leases WHERE resource = $1",
            )
            .bind(resource)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
            let Some(current) = current else {
                // Nobody has held it yet: the first insert wins term 1.
                let row = sqlx::query(
                    "INSERT INTO leases (resource, owner, expires_at, term)
                     VALUES ($1, $2, (extract(epoch from now()) * 1000)::bigint + $3, 1)
                     ON CONFLICT (resource) DO NOTHING
                     RETURNING owner, expires_at, term",
                )
                .bind(resource)
                .bind(owner)
                .bind(ttl_ms)
                .fetch_optional(self.pool())
                .await
                .map_err(db_err)?;
                match row {
                    Some(r) => return Ok(lease_of(&r)),
                    None => continue,
                }
            };
            let holder: String = current.get("owner");
            let expired: bool = current.get("expired");
            if holder != owner && !expired {
                return Ok(lease_of(&current));
            }
            // The holder's renewal (the term stays: an expired lease nobody
            // took over is still this owner's term), or a takeover of an
            // expired lease (the next term). The renewal of a constantly
            // re-leasing leader must EXTEND `expires_at`, or it expires after
            // one TTL and a peer steals leadership at the boundary.
            //
            // The UPDATE waits for an in-flight fenced transaction of the
            // holder; that wait is the fence (a takeover lands after the
            // stale leader's commit, or after Postgres rolled it back). It is
            // bounded by TAKEOVER_WAIT: past it the holder is reported
            // unchanged and the caller tries again next tick, by when a
            // stopped holder's transaction has hit its own bound.
            let mut tx = self.pool().begin().await.map_err(db_err)?;
            sqlx::query(&format!("SET LOCAL lock_timeout = '{TAKEOVER_WAIT}'"))
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            let row = sqlx::query(
                "UPDATE leases
                 SET owner = $2,
                     expires_at = (extract(epoch from now()) * 1000)::bigint + $3,
                     term = CASE WHEN owner = $2 THEN term ELSE term + 1 END
                 WHERE resource = $1 AND owner = $4 AND term = $5
                   AND (owner = $2 OR expires_at < (extract(epoch from now()) * 1000)::bigint)
                 RETURNING owner, expires_at, term",
            )
            .bind(resource)
            .bind(owner)
            .bind(ttl_ms)
            .bind(&holder)
            .bind(current.get::<i64, _>("term"))
            .fetch_optional(&mut *tx)
            .await;
            match row {
                Ok(Some(r)) => {
                    tx.commit().await.map_err(db_err)?;
                    return Ok(lease_of(&r));
                }
                // The row moved since the read (a renewal or a takeover won).
                Ok(None) => continue,
                // lock_not_available: the holder is inside a fenced
                // transaction. Report it as it was read.
                Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("55P03") => {
                    return Ok(lease_of(&current));
                }
                Err(e) => return Err(db_err(e)),
            }
        }
        // Three lost races in a row: report whoever holds it now.
        let r = sqlx::query("SELECT owner, expires_at, term FROM leases WHERE resource = $1")
            .bind(resource)
            .fetch_one(self.pool())
            .await
            .map_err(db_err)?;
        Ok(lease_of(&r))
    }
}

/// A [`Lease`] from a `leases` row (owner, expires_at, term).
fn lease_of(r: &sqlx::postgres::PgRow) -> Lease {
    Lease {
        owner: r.get::<String, _>("owner"),
        expires_at: Timestamp(r.get::<i64, _>("expires_at")),
        term: r.get::<i64, _>("term"),
    }
}

#[async_trait]
impl ForgeConnectionStore for PostgresDb {
    async fn put_connection(&self, conn: &ForgeConnection) -> Result<(), RegistryError> {
        let mut tx = self.pool().begin().await.map_err(reg_err)?;
        // The connection's Org enters the Org record (git-bug 19b0a45).
        if let Some(org) = &conn.org {
            sqlx::query("INSERT INTO orgs (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
                .bind(org)
                .execute(&mut *tx)
                .await
                .map_err(reg_err)?;
        }
        // A known account id is kept when the writer does not know it (boot
        // provisioning, the connection form): it is a fact about the forge
        // account, recorded only from an installation delivery.
        sqlx::query(
            "INSERT INTO forge_connections
                 (id, kind, base_url, credential_ref, org, installation_id, account_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO UPDATE SET
                 kind = EXCLUDED.kind,
                 base_url = EXCLUDED.base_url,
                 credential_ref = EXCLUDED.credential_ref,
                 org = EXCLUDED.org,
                 installation_id = EXCLUDED.installation_id,
                 account_id = COALESCE(EXCLUDED.account_id, forge_connections.account_id)",
        )
        .bind(&conn.id)
        .bind(conn.kind.as_str())
        .bind(&conn.base_url)
        .bind(&conn.credential_ref)
        .bind(&conn.org)
        .bind(conn.installation_id.map(|i| i as i64))
        .bind(conn.account_id.map(|i| i as i64))
        .execute(&mut *tx)
        .await
        .map_err(reg_err)?;
        tx.commit().await.map_err(reg_err)?;
        Ok(())
    }

    async fn get_connection(&self, id: &str) -> Result<Option<ForgeConnection>, RegistryError> {
        let row = sqlx::query(
            "SELECT id, kind, base_url, credential_ref, org, installation_id, account_id
             FROM forge_connections WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(reg_err)?;
        row.map(connection_from_row).transpose()
    }

    async fn list_connections(&self) -> Result<Vec<ForgeConnection>, RegistryError> {
        let rows = sqlx::query(
            "SELECT id, kind, base_url, credential_ref, org, installation_id, account_id
             FROM forge_connections ORDER BY id",
        )
        .fetch_all(self.pool())
        .await
        .map_err(reg_err)?;
        rows.into_iter().map(connection_from_row).collect()
    }

    async fn delete_connection(&self, id: &str) -> Result<(), RegistryError> {
        // Repo bindings go with it (ON DELETE CASCADE).
        sqlx::query("DELETE FROM forge_connections WHERE id = $1")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(reg_err)?;
        Ok(())
    }

    async fn upsert_repo_binding(
        &self,
        connection_id: &str,
        repo: &RepoRef,
        org: &str,
        project: &str,
    ) -> Result<(), RegistryError> {
        let mut tx = self.pool().begin().await.map_err(reg_err)?;
        // Held to commit: an assign of this connection, or of any connection
        // to this Org, waits for the bind, and the bind waits for it. The
        // connection's Org is read under the row lock, so a bind cannot land
        // a repo of another owner on a connection being assigned
        // (git-bug 19b0a45).
        lock_org(&mut tx, org).await?;
        let conn_org: Option<Option<String>> =
            sqlx::query_scalar("SELECT org FROM forge_connections WHERE id = $1 FOR SHARE")
                .bind(connection_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(reg_err)?;
        if let Some(Some(conn_org)) = conn_org {
            if repo.owner != conn_org || org != conn_org {
                return Err(RegistryError::OrgMismatch {
                    connection: connection_id.to_string(),
                    connection_org: conn_org,
                    owner: repo.owner.clone(),
                    name: repo.name.clone(),
                });
            }
        }
        sqlx::query("INSERT INTO orgs (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
            .bind(org)
            .execute(&mut *tx)
            .await
            .map_err(reg_err)?;
        // A re-bind moves the repo only off a connection that does not serve
        // its owner: an unassigned one, or one of another Org that holds it
        // by mistake (git-bug 19b0a45). The guard is the upsert's own
        // `WHERE`, so it is evaluated against the row as it stands when the
        // write happens: two
        // concurrent binds of an unbound repo cannot both win, and a move
        // checked against a stale read cannot slip through.
        let written = sqlx::query(
            "INSERT INTO forge_repos (connection_id, owner, name, org, project)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (owner, name) DO UPDATE SET
                 connection_id = EXCLUDED.connection_id,
                 org = EXCLUDED.org,
                 project = EXCLUDED.project,
                 unreachable_since = NULL
             WHERE forge_repos.connection_id = EXCLUDED.connection_id
                OR NOT EXISTS (
                    SELECT 1 FROM forge_connections c
                     WHERE c.id = forge_repos.connection_id AND c.org = forge_repos.owner)",
        )
        .bind(connection_id)
        .bind(&repo.owner)
        .bind(&repo.name)
        .bind(org)
        .bind(project)
        .execute(&mut *tx)
        .await
        .map_err(reg_err)?;
        if written.rows_affected() == 0 {
            let holder: String = sqlx::query_scalar(
                "SELECT connection_id FROM forge_repos WHERE owner = $1 AND name = $2",
            )
            .bind(&repo.owner)
            .bind(&repo.name)
            .fetch_one(&mut *tx)
            .await
            .map_err(reg_err)?;
            return Err(RegistryError::BoundElsewhere {
                owner: repo.owner.clone(),
                name: repo.name.clone(),
                connection: holder,
            });
        }
        tx.commit().await.map_err(reg_err)?;
        Ok(())
    }

    async fn unbind_repo(&self, connection_id: &str, repo: &RepoRef) -> Result<(), RegistryError> {
        sqlx::query(
            "DELETE FROM forge_repos WHERE connection_id = $1 AND owner = $2 AND name = $3",
        )
        .bind(connection_id)
        .bind(&repo.owner)
        .bind(&repo.name)
        .execute(self.pool())
        .await
        .map_err(reg_err)?;
        Ok(())
    }

    async fn mark_repo_unreachable(
        &self,
        connection_id: &str,
        repo: &RepoRef,
        at: i64,
    ) -> Result<(), RegistryError> {
        sqlx::query(
            "UPDATE forge_repos SET unreachable_since = COALESCE(unreachable_since, $4)
             WHERE connection_id = $1 AND owner = $2 AND name = $3",
        )
        .bind(connection_id)
        .bind(&repo.owner)
        .bind(&repo.name)
        .bind(at)
        .execute(self.pool())
        .await
        .map_err(reg_err)?;
        Ok(())
    }

    async fn clear_repo_unreachable(
        &self,
        connection_id: &str,
        repo: &RepoRef,
    ) -> Result<(), RegistryError> {
        sqlx::query(
            "UPDATE forge_repos SET unreachable_since = NULL
             WHERE connection_id = $1 AND owner = $2 AND name = $3
               AND unreachable_since IS NOT NULL",
        )
        .bind(connection_id)
        .bind(&repo.owner)
        .bind(&repo.name)
        .execute(self.pool())
        .await
        .map_err(reg_err)?;
        Ok(())
    }

    async fn repos_of(&self, connection_id: &str) -> Result<Vec<RepoRef>, RegistryError> {
        let rows = sqlx::query(
            "SELECT owner, name FROM forge_repos WHERE connection_id = $1 ORDER BY owner, name",
        )
        .bind(connection_id)
        .fetch_all(self.pool())
        .await
        .map_err(reg_err)?;
        Ok(rows
            .into_iter()
            .map(|r| RepoRef {
                owner: r.get::<String, _>("owner"),
                name: r.get::<String, _>("name"),
            })
            .collect())
    }

    async fn repo_count_of(&self, connection_id: &str) -> Result<u64, RegistryError> {
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM forge_repos WHERE connection_id = $1")
                .bind(connection_id)
                .fetch_one(self.pool())
                .await
                .map_err(reg_err)?;
        Ok(n as u64)
    }

    async fn resolve(&self, repo: &RepoRef) -> Result<Option<ResolvedRepo>, RegistryError> {
        let row = sqlx::query(
            "SELECT c.id, c.kind, c.base_url, c.credential_ref, c.org AS conn_org,
                    c.installation_id, c.account_id, r.org, r.project, r.unreachable_since
             FROM forge_repos r JOIN forge_connections c ON c.id = r.connection_id
             WHERE r.owner = $1 AND r.name = $2",
        )
        .bind(&repo.owner)
        .bind(&repo.name)
        .fetch_optional(self.pool())
        .await
        .map_err(reg_err)?;
        row.map(|r| {
            Ok(ResolvedRepo {
                org: r.get::<String, _>("org"),
                project: r.get::<String, _>("project"),
                unreachable_since: r.get::<Option<i64>, _>("unreachable_since"),
                connection: connection_from_row(r)?,
            })
        })
        .transpose()
    }

    async fn record_delivery(
        &self,
        forge: ForgeKind,
        delivery_id: &str,
    ) -> Result<bool, RegistryError> {
        // First writer wins; a conflicting insert (a replay) affects zero rows.
        let result = sqlx::query(
            "INSERT INTO webhook_deliveries (forge, id, at)
             VALUES ($1, $2, (extract(epoch from now()) * 1000)::bigint)
             ON CONFLICT (forge, id) DO NOTHING",
        )
        .bind(forge.as_str())
        .bind(delivery_id)
        .execute(self.pool())
        .await
        .map_err(reg_err)?;
        Ok(result.rows_affected() == 1)
    }

    async fn last_delivery_at(&self, forge: ForgeKind) -> Result<Option<i64>, RegistryError> {
        let row = sqlx::query("SELECT max(at) AS at FROM webhook_deliveries WHERE forge = $1")
            .bind(forge.as_str())
            .fetch_one(self.pool())
            .await
            .map_err(reg_err)?;
        Ok(row.get::<Option<i64>, _>("at"))
    }

    /// Single-owner marker (ADR-0060 part D). Ownership is a property of the row,
    /// not of the connection identity, so it is set separately from
    /// `put_connection` — boot provisioning upserts the connection and then
    /// claims it, and *releasing* it (config stopped declaring it) is the same
    /// call with `false`, which never touches the connection's own fields.
    async fn set_connection_owned_by_config(
        &self,
        id: &str,
        owned: bool,
    ) -> Result<(), RegistryError> {
        sqlx::query("UPDATE forge_connections SET owned_by_config = $2 WHERE id = $1")
            .bind(id)
            .bind(owned)
            .execute(self.pool())
            .await
            .map_err(reg_err)?;
        Ok(())
    }

    /// The `orgs` table (git-bug 19b0a45): an Org outlives its last binding.
    /// The Orgs of bindings are added: during a rolling upgrade a server
    /// built before the `orgs` table binds repos without recording their Org.
    async fn list_orgs(&self) -> Result<Vec<String>, RegistryError> {
        sqlx::query_scalar(
            "SELECT name FROM orgs
             UNION
             SELECT DISTINCT org FROM forge_repos WHERE org NOT LIKE '\\_%'
             ORDER BY 1",
        )
        .fetch_all(self.pool())
        .await
        .map_err(reg_err)
    }

    async fn record_org(&self, org: &str) -> Result<(), RegistryError> {
        if org.is_empty() || scarab_forge::is_reserved_org(org) {
            return Ok(());
        }
        sqlx::query("INSERT INTO orgs (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
            .bind(org)
            .execute(self.pool())
            .await
            .map_err(reg_err)?;
        Ok(())
    }

    async fn set_connection_account(&self, id: &str, account_id: u64) -> Result<(), RegistryError> {
        sqlx::query("UPDATE forge_connections SET account_id = $2 WHERE id = $1")
            .bind(id)
            .bind(account_id as i64)
            .execute(self.pool())
            .await
            .map_err(reg_err)?;
        Ok(())
    }

    /// One transaction: the Org's advisory lock, then the connection row
    /// `FOR UPDATE`, then the reads `decide` is asked about, then the write.
    async fn assign_connection_org(
        &self,
        id: &str,
        org: &str,
        decide: &(dyn for<'v> Fn(&'v OrgAssignView) -> Option<String> + Send + Sync),
    ) -> Result<Result<ForgeConnection, String>, RegistryError> {
        let mut tx = self.pool().begin().await.map_err(reg_err)?;
        lock_org(&mut tx, org).await?;
        let conn = sqlx::query(
            "SELECT id, kind, base_url, credential_ref, org, installation_id, account_id
             FROM forge_connections WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reg_err)?
        .map(connection_from_row)
        .transpose()?
        .ok_or_else(|| RegistryError::Store(format!("no connection `{id}`")))?;
        let bindings = sqlx::query(
            "SELECT owner, name, org FROM forge_repos WHERE connection_id = $1
             ORDER BY owner, name",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .map_err(reg_err)?
        .into_iter()
        .map(|r| {
            (
                RepoRef {
                    owner: r.get::<String, _>("owner"),
                    name: r.get::<String, _>("name"),
                },
                r.get::<String, _>("org"),
            )
        })
        .collect();
        let servers = sqlx::query(
            "SELECT id, kind, base_url, credential_ref, org, installation_id, account_id
             FROM forge_connections c
             WHERE c.id <> $1
               AND (c.org = $2
                    OR (c.org IS NULL AND EXISTS (
                        SELECT 1 FROM forge_repos r
                         WHERE r.connection_id = c.id AND r.org = $2)))
             ORDER BY id",
        )
        .bind(id)
        .bind(org)
        .fetch_all(&mut *tx)
        .await
        .map_err(reg_err)?
        .into_iter()
        .map(connection_from_row)
        .collect::<Result<Vec<_>, _>>()?;
        let org_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM orgs WHERE name = $1)
                 OR EXISTS (SELECT 1 FROM forge_repos WHERE org = $1)",
        )
        .bind(org)
        .fetch_one(&mut *tx)
        .await
        .map_err(reg_err)?;
        let view = OrgAssignView {
            conn,
            bindings,
            servers,
            org_exists,
        };
        if let Some(reason) = decide(&view) {
            return Ok(Err(reason));
        }
        sqlx::query("UPDATE forge_connections SET org = $2 WHERE id = $1")
            .bind(id)
            .bind(org)
            .execute(&mut *tx)
            .await
            .map_err(reg_err)?;
        sqlx::query("INSERT INTO orgs (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
            .bind(org)
            .execute(&mut *tx)
            .await
            .map_err(reg_err)?;
        tx.commit().await.map_err(reg_err)?;
        Ok(Ok(ForgeConnection {
            org: Some(org.to_string()),
            ..view.conn
        }))
    }

    async fn config_owned_connection_ids(&self) -> Result<Vec<String>, RegistryError> {
        let rows =
            sqlx::query("SELECT id FROM forge_connections WHERE owned_by_config ORDER BY id")
                .fetch_all(self.pool())
                .await
                .map_err(reg_err)?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>("id")).collect())
    }
}

fn connection_from_row(r: sqlx::postgres::PgRow) -> Result<ForgeConnection, RegistryError> {
    let kind = r.get::<String, _>("kind");
    // `resolve` joins the binding's own `org`, so it names the connection's
    // `conn_org`; every other query selects it as `org`.
    let org = match r.try_get::<Option<String>, _>("conn_org") {
        Ok(org) => org,
        Err(_) => r.get::<Option<String>, _>("org"),
    };
    Ok(ForgeConnection {
        id: r.get::<String, _>("id"),
        kind: ForgeKind::from_str_token(&kind)
            .ok_or_else(|| RegistryError::Store(format!("unknown forge kind {kind:?}")))?,
        base_url: r.get::<String, _>("base_url"),
        credential_ref: r.get::<String, _>("credential_ref"),
        org,
        installation_id: r
            .get::<Option<i64>, _>("installation_id")
            .and_then(|i| u64::try_from(i).ok()),
        account_id: r
            .get::<Option<i64>, _>("account_id")
            .and_then(|i| u64::try_from(i).ok()),
    })
}

/// The transaction-scoped advisory lock every change to who serves `org`
/// takes (git-bug 19b0a45): an assign of a connection to it and a bind of a
/// repo into it. They then run one after the other, so a check one of them
/// makes cannot be undone by the other before it writes.
async fn lock_org(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: &str,
) -> Result<(), RegistryError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("scarab:org:{org}"))
        .execute(&mut **tx)
        .await
        .map_err(reg_err)?;
    Ok(())
}

fn reg_err(e: sqlx::Error) -> RegistryError {
    RegistryError::Store(e.to_string())
}

/// "Intentionally unset" markers for the advisory coverage matrix (ADR-0037 D).
/// The repo-default column is stored as `environment = ''` (see migration 0037).
#[async_trait]
impl scarab_project::SecretCoverageStore for PostgresDb {
    async fn silence(
        &self,
        org: &str,
        project: &str,
        column: scarab_project::CoverageColumn<'_>,
        key: &str,
    ) -> Result<(), ProjectError> {
        sqlx::query(
            "INSERT INTO secret_unset_markers (org, project, environment, key)
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(org)
        .bind(project)
        .bind(column.unwrap_or(""))
        .bind(key)
        .execute(self.pool())
        .await
        .map_err(|e| ProjectError::Store(e.to_string()))?;
        Ok(())
    }

    async fn unsilence(
        &self,
        org: &str,
        project: &str,
        column: scarab_project::CoverageColumn<'_>,
        key: &str,
    ) -> Result<(), ProjectError> {
        sqlx::query(
            "DELETE FROM secret_unset_markers
             WHERE org = $1 AND project = $2 AND environment = $3 AND key = $4",
        )
        .bind(org)
        .bind(project)
        .bind(column.unwrap_or(""))
        .bind(key)
        .execute(self.pool())
        .await
        .map_err(|e| ProjectError::Store(e.to_string()))?;
        Ok(())
    }

    async fn silenced(
        &self,
        org: &str,
        project: &str,
    ) -> Result<Vec<(Option<String>, String)>, ProjectError> {
        let rows = sqlx::query(
            "SELECT environment, key FROM secret_unset_markers
             WHERE org = $1 AND project = $2",
        )
        .bind(org)
        .bind(project)
        .fetch_all(self.pool())
        .await
        .map_err(|e| ProjectError::Store(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let env = r.get::<String, _>("environment");
                let column = (!env.is_empty()).then_some(env);
                (column, r.get::<String, _>("key"))
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Codecs: domain enums <-> the small, stable strings stored in TEXT columns.
// Kept explicit (rather than leaning on serde) so the on-disk vocabulary is
// visible and independent of Rust identifier spelling.
// ---------------------------------------------------------------------------

/// The first row of a Run whose creation has begun (`creating = TRUE`):
/// [`Db::begin_run_creation`], and the `run.create` decision's transaction
/// (git-bug bbc433e).
pub(crate) async fn insert_creating_run<'e, E>(
    executor: E,
    run_id: &str,
    ir_version: u32,
    event_schema_version: u32,
    at: i64,
) -> Result<(), sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query(
        "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at, updated_at,
                           creating)
         VALUES ($1, $2, $3, $4, $5, $5, TRUE)",
    )
    .bind(run_id)
    .bind(run_status_str(RunStatus::Pending))
    .bind(ir_version as i32)
    .bind(event_schema_version as i32)
    .bind(at)
    .execute(executor)
    .await?;
    Ok(())
}

fn run_status_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::Suspended => "suspended",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::DeadLettered => "dead_lettered",
    }
}

/// Map a `runs` row (as selected by the two `list_runs*` queries) into a
/// [`RunSummary`], including the tenancy and origin projections. Shared so the
/// column set stays in lock-step between the global and per-tenant lists.
fn run_summary_from_row(r: sqlx::postgres::PgRow) -> Result<RunSummary, DbError> {
    let tenant = match (
        r.get::<Option<String>, _>("tenant_org"),
        r.get::<Option<String>, _>("tenant_project"),
    ) {
        (Some(o), Some(p)) => Some((o, p)),
        _ => None,
    };
    Ok(RunSummary {
        run: RunId(r.get::<String, _>("id")),
        status: run_status_from_str(r.get::<String, _>("status"))?,
        created_at: Timestamp(r.get::<i64, _>("created_at")),
        updated_at: Timestamp(r.get::<i64, _>("updated_at")),
        tenant,
        run_number: r.get::<Option<i64>, _>("run_number"),
        trigger_kind: r.get::<Option<String>, _>("origin_trigger_kind"),
        actor: r.get::<Option<String>, _>("origin_actor"),
        git_ref: r.get::<Option<String>, _>("origin_ref"),
        sha: r.get::<Option<String>, _>("origin_sha"),
        pr_number: r.get::<Option<i64>, _>("origin_pr_number"),
        pr_base: r.get::<Option<String>, _>("origin_pr_base"),
        pipeline: r.get::<Option<String>, _>("pipeline"),
        trigger_title: r.get::<Option<String>, _>("trigger_title"),
        environment: r.get::<Option<String>, _>("deploy_environment"),
        deploy: r.get::<bool, _>("is_deploy_run"),
    })
}

fn run_status_from_str(s: String) -> Result<RunStatus, DbError> {
    Ok(match s.as_str() {
        "pending" => RunStatus::Pending,
        "running" => RunStatus::Running,
        "suspended" => RunStatus::Suspended,
        "succeeded" => RunStatus::Succeeded,
        "failed" => RunStatus::Failed,
        "cancelled" => RunStatus::Cancelled,
        "dead_lettered" => RunStatus::DeadLettered,
        other => return Err(DbError::Other(format!("unknown run status {other:?}"))),
    })
}

/// Serialize a step's dependency edges to the JSONB array stored in
/// `step_runs.needs` (an array of step-id strings).
fn needs_to_value(needs: &[StepId]) -> Value {
    Value::Array(needs.iter().map(|s| Value::String(s.0.clone())).collect())
}

/// Parse the `step_runs.needs` JSONB array back into step ids.
fn needs_from_value(v: Value) -> Result<Vec<StepId>, DbError> {
    let ids: Vec<String> = serde_json::from_value(v).map_err(|e| DbError::Other(e.to_string()))?;
    Ok(ids.into_iter().map(StepId).collect())
}

fn step_status_str(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Pending => "pending",
        StepStatus::Ready => "ready",
        StepStatus::Running => "running",
        StepStatus::Succeeded => "succeeded",
        StepStatus::Failed => "failed",
        StepStatus::Skipped => "skipped",
        StepStatus::Cancelled => "cancelled",
    }
}

fn step_status_from_str(s: String) -> Result<StepStatus, DbError> {
    Ok(match s.as_str() {
        "pending" => StepStatus::Pending,
        "ready" => StepStatus::Ready,
        "running" => StepStatus::Running,
        "succeeded" => StepStatus::Succeeded,
        "failed" => StepStatus::Failed,
        "skipped" => StepStatus::Skipped,
        "cancelled" => StepStatus::Cancelled,
        other => return Err(DbError::Other(format!("unknown step status {other:?}"))),
    })
}

fn failure_str(f: FailureKind) -> &'static str {
    match f {
        // "infra" keeps its pre-ADR-0047 spelling (post-start is the
        // conservative reading of historical rows).
        FailureKind::Infra {
            never_started: false,
        } => "infra",
        FailureKind::Infra {
            never_started: true,
        } => "infra-never-started",
        FailureKind::Step => "step",
        FailureKind::Timeout => "timeout",
        FailureKind::Lost => "lost",
        FailureKind::Config => "config",
        // Forward-only (ticket e140121): rows carrying this string are
        // unreadable to a binary from before the variant — deliberate, no
        // unknown-variant fallback (see `FailureKind::MissingInputs`).
        FailureKind::MissingInputs => "missing-inputs",
        // Forward-only, like "missing-inputs": a gate that resolved failed
        // (slice b, criterion 3).
        FailureKind::Gate => "gate",
    }
}

fn failure_from_str(s: &str) -> Result<FailureKind, DbError> {
    match s {
        "infra" => Ok(FailureKind::Infra {
            never_started: false,
        }),
        "infra-never-started" => Ok(FailureKind::Infra {
            never_started: true,
        }),
        "step" => Ok(FailureKind::Step),
        "timeout" => Ok(FailureKind::Timeout),
        "lost" => Ok(FailureKind::Lost),
        "config" => Ok(FailureKind::Config),
        "missing-inputs" => Ok(FailureKind::MissingInputs),
        "gate" => Ok(FailureKind::Gate),
        // Retired by ADR-0074 with the Deployment record. Rows written before
        // then still read back — as `step`, like the event log's alias: a
        // verdict on a Step that ran, never auto-retried. Their
        // `failure_detail` keeps the original reason.
        "deployment-request-incomplete" => Ok(FailureKind::Step),
        other => Err(DbError::Other(format!("unknown failure kind {other:?}"))),
    }
}

/// [`PostgresDb::attempts`] on any executor — the pool, or an open
/// transaction (the gate-wait writes read a step's Attempts under its row
/// lock). Same order: the numeric id suffix alone.
async fn attempts_on<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    run: &RunId,
    step: &StepId,
) -> Result<Vec<Attempt>, DbError> {
    let rows = sqlx::query(
        "SELECT attempt_id, started_at, failure, failure_detail, outcome
         FROM attempts
         WHERE run_id = $1 AND step_id = $2
         ORDER BY CAST(substring(attempt_id FROM 2) AS INTEGER)",
    )
    .bind(&run.0)
    .bind(&step.0)
    .fetch_all(exec)
    .await
    .map_err(db_err)?;
    rows.iter().map(attempt_from_row).collect()
}

/// One `attempts` row (`attempt_id, started_at, failure, failure_detail,
/// outcome`) as an [`Attempt`].
fn attempt_from_row(r: &sqlx::postgres::PgRow) -> Result<Attempt, DbError> {
    let failure = r
        .get::<Option<String>, _>("failure")
        .map(|s| failure_from_str(&s))
        .transpose()?;
    // Back-compat derivation lives at this storage boundary — the only place
    // the NULL `outcome` column is observed — so the in-memory
    // `Attempt.outcome` is authoritative for every consumer. A pre-migration
    // row (NULL outcome) derives `Failed` when a failure was recorded, else
    // `Running`.
    let outcome = match r.get::<Option<String>, _>("outcome") {
        Some(s) => AttemptOutcome::from_str(&s)
            .ok_or_else(|| DbError::Other(format!("unknown attempt outcome {s:?}")))?,
        None if failure.is_some() => AttemptOutcome::Failed,
        None => AttemptOutcome::Running,
    };
    Ok(Attempt {
        id: AttemptId(r.get::<String, _>("attempt_id")),
        started_at: Timestamp(r.get::<i64, _>("started_at")),
        failure,
        failure_detail: r.get::<Option<String>, _>("failure_detail"),
        outcome,
    })
}

/// SQL predicate: the `attempts` row is still in flight. A pre-migration row
/// (NULL `outcome`) is running unless it recorded a failure — the same
/// derivation [`attempts_on`] applies on read.
const ATTEMPT_IS_RUNNING: &str = "(outcome = 'running' OR (outcome IS NULL AND failure IS NULL))";

/// The guarded run-status CAS on any executor: `true` iff the row was in
/// `from` and is now `to`. Zero rows means a concurrent/duplicate writer
/// already moved it. Moving a run OUT of a terminal status (a reopen) bumps
/// its `reopen_generation` in the same write ([`Db::run_reopen_generation`]).
async fn cas_run_status<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    run: &RunId,
    from: RunStatus,
    to: RunStatus,
    generation: Option<u64>,
) -> Result<bool, DbError> {
    // `generation`, when given, is the reopen generation the caller read:
    // a run reopened since (and back in `from`) is a miss, not an ABA hit.
    let affected = sqlx::query(
        "UPDATE runs
         SET status = $3, updated_at = (extract(epoch from now()) * 1000)::bigint,
             reopen_generation = reopen_generation + CASE WHEN $4 THEN 1 ELSE 0 END
         WHERE id = $1 AND status = $2
           AND ($5::BIGINT IS NULL OR reopen_generation = $5)",
    )
    .bind(&run.0)
    .bind(run_status_str(from))
    .bind(run_status_str(to))
    .bind(from.is_terminal())
    .bind(generation.map(|g| g as i64))
    .execute(exec)
    .await
    .map_err(db_err)?
    .rows_affected();
    Ok(affected > 0)
}

/// Enqueue one outbox message on any executor. Unique idempotency_key: a
/// retried enqueue collapses to the existing row, so the logical effect is
/// enqueued exactly once.
async fn insert_outbox<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    msg: &OutboxMessage,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO outbox (run_id, kind, payload, idempotency_key, created_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (idempotency_key) DO NOTHING",
    )
    .bind(&msg.run.0)
    .bind(&msg.kind)
    .bind(&msg.payload)
    .bind(&msg.idempotency_key)
    .bind(msg.at.0)
    .execute(exec)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Append one event on any executor (the pool, or an open transaction).
/// The guarded step-status CAS on any executor: `true` iff the row was in
/// `from` and is now `to`.
async fn cas_step_status<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    run: &RunId,
    step: &StepId,
    from: StepStatus,
    to: StepStatus,
) -> Result<bool, DbError> {
    let affected = sqlx::query(
        "UPDATE step_runs
         SET status = $4, updated_at = (extract(epoch from now()) * 1000)::bigint
         WHERE run_id = $1 AND step_id = $2 AND status = $3",
    )
    .bind(&run.0)
    .bind(&step.0)
    .bind(step_status_str(from))
    .bind(step_status_str(to))
    .execute(exec)
    .await
    .map_err(db_err)?
    .rows_affected();
    Ok(affected > 0)
}

/// [`Db::set_attempt_outcome`] on any executor (see it for the
/// terminal-by-intent guard).
async fn update_attempt_outcome<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
    outcome: AttemptOutcome,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE attempts SET outcome = $4
         WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3
           AND outcome IS DISTINCT FROM 'superseded'
           AND outcome IS DISTINCT FROM 'cancelled'",
    )
    .bind(&run.0)
    .bind(&step.0)
    .bind(&attempt.0)
    .bind(outcome.as_str())
    .execute(exec)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// [`Db::set_attempt_failure`] on any executor (see it for the guard and
/// why failure, cause and outcome move together).
async fn update_attempt_failure<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
    failure: FailureKind,
    detail: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE attempts SET failure = $4, failure_detail = $5, outcome = $6
         WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3
           AND outcome IS DISTINCT FROM 'superseded'
           AND outcome IS DISTINCT FROM 'cancelled'",
    )
    .bind(&run.0)
    .bind(&step.0)
    .bind(&attempt.0)
    .bind(failure_str(failure))
    .bind(detail)
    .bind(AttemptOutcome::Failed.as_str())
    .execute(exec)
    .await
    .map_err(db_err)?;
    Ok(())
}

async fn insert_event<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    event: &EventKind,
) -> Result<(), DbError> {
    let payload = serde_json::to_value(&event.kind).map_err(|e| DbError::Other(e.to_string()))?;
    sqlx::query("INSERT INTO events (run_id, version, at, payload) VALUES ($1, $2, $3, $4)")
        .bind(&event.run.0)
        .bind(event.version as i32)
        .bind(event.at.0)
        .bind(payload)
        .execute(exec)
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Lock step `step`'s row for the rest of `conn`'s transaction and report
/// whether it is `Pending` — the serialisation point of every gate-wait write.
async fn lock_pending_step(
    conn: &mut sqlx::PgConnection,
    run: &RunId,
    step: &StepId,
) -> Result<bool, DbError> {
    let row =
        sqlx::query("SELECT status FROM step_runs WHERE run_id = $1 AND step_id = $2 FOR UPDATE")
            .bind(&run.0)
            .bind(&step.0)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db_err)?;
    Ok(row.is_some_and(|r| r.get::<String, _>("status") == step_status_str(StepStatus::Pending)))
}

/// The next attempt number: one past the highest `a{n}` minted so far.
fn next_attempt_number(attempts: &[Attempt]) -> u64 {
    attempts
        .iter()
        .filter_map(|a| a.id.0.strip_prefix('a').and_then(|n| n.parse::<u64>().ok()))
        .max()
        .unwrap_or(0)
        + 1
}

/// Insert a fresh `attempts` row on `conn` (no upsert: a gate-wait write
/// mints under the step-row lock, so its id is new).
async fn insert_attempt(
    conn: &mut sqlx::PgConnection,
    run: &RunId,
    step: &StepId,
    attempt: &Attempt,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO attempts
             (run_id, step_id, attempt_id, started_at, failure, failure_detail, outcome)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&run.0)
    .bind(&step.0)
    .bind(&attempt.id.0)
    .bind(attempt.started_at.0)
    .bind(attempt.failure.map(failure_str))
    .bind(attempt.failure_detail.as_deref())
    .bind(attempt.outcome.as_str())
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Close attempt `attempt` with `outcome` (and its failure class and detail)
/// on `conn`, only while it is still running. Returns whether it closed.
async fn close_attempt_on(
    conn: &mut sqlx::PgConnection,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
    outcome: AttemptOutcome,
    failure: Option<FailureKind>,
    detail: Option<&str>,
) -> Result<bool, DbError> {
    let closed = sqlx::query(&format!(
        "UPDATE attempts SET outcome = $4, failure = $5, failure_detail = $6
         WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3 AND {ATTEMPT_IS_RUNNING}"
    ))
    .bind(&run.0)
    .bind(&step.0)
    .bind(&attempt.0)
    .bind(outcome.as_str())
    .bind(failure.map(failure_str))
    .bind(detail)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?
    .rows_affected();
    Ok(closed == 1)
}

fn db_err(e: sqlx::Error) -> DbError {
    match e {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => DbError::Unavailable,
        // A bound passed: only leader-fenced transactions set one
        // (`PostgresDb::begin_fenced`; `lease` handles its own lock wait),
        // and Postgres rolled the transaction back.
        sqlx::Error::Database(d)
            if matches!(
                d.code().as_deref(),
                Some(LOCK_NOT_AVAILABLE | QUERY_CANCELED | IDLE_IN_TRANSACTION_TIMEOUT)
            ) =>
        {
            DbError::FenceTimeout
        }
        other => DbError::Other(other.to_string()),
    }
}

/// SQLSTATE `lock_not_available`: a lock wait passed `lock_timeout`.
const LOCK_NOT_AVAILABLE: &str = "55P03";
/// SQLSTATE `query_canceled`: a statement passed `statement_timeout`.
const QUERY_CANCELED: &str = "57014";
/// SQLSTATE `idle_in_transaction_session_timeout`: Postgres ended a session
/// that sat idle inside a transaction past the bound.
const IDLE_IN_TRANSACTION_TIMEOUT: &str = "25P03";

// ---------------------------------------------------------------------------
// SessionStore (ADR-0049 C1): server-side login sessions in Postgres.
//
// Keyed by `session_id_hash(id)`, never the id; `csrf` holds the CSRF token's
// hash (migration 0064, git-bug 51d6bcb). A row read from the table rides no
// session.
// ---------------------------------------------------------------------------

#[async_trait]
impl scarab_identity::SessionStore for PostgresDb {
    async fn put(
        &self,
        session: &scarab_identity::Session,
    ) -> Result<(), scarab_identity::IdentityError> {
        let principal = serde_json::to_value(&session.principal)
            .map_err(|e| scarab_identity::IdentityError::Issuance(e.to_string()))?;
        sqlx::query(
            "INSERT INTO sessions (id, principal, csrf, expires_at, created_at, login_kind)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO UPDATE SET
                 principal = EXCLUDED.principal,
                 csrf = EXCLUDED.csrf,
                 expires_at = EXCLUDED.expires_at,
                 created_at = EXCLUDED.created_at,
                 login_kind = EXCLUDED.login_kind",
        )
        .bind(scarab_identity::session_id_hash(&session.id))
        .bind(&principal)
        .bind(&session.csrf_hash)
        .bind(session.expires_at)
        .bind(session.created_at)
        .bind(session.login_kind.as_str())
        .execute(self.pool())
        .await
        .map_err(|e| scarab_identity::IdentityError::Issuance(e.to_string()))?;
        Ok(())
    }

    async fn get(
        &self,
        id: &str,
    ) -> Result<Option<scarab_identity::Session>, scarab_identity::IdentityError> {
        let row = sqlx::query(
            "SELECT principal, csrf, expires_at, created_at, login_kind FROM sessions WHERE id = $1",
        )
        .bind(scarab_identity::session_id_hash(id))
            .fetch_optional(self.pool())
            .await
            .map_err(|e| scarab_identity::IdentityError::Issuance(e.to_string()))?;
        let Some(row) = row else { return Ok(None) };
        let principal: scarab_identity::Principal =
            serde_json::from_value(row.get::<serde_json::Value, _>("principal"))
                .map_err(|e| scarab_identity::IdentityError::Issuance(e.to_string()))?;
        Ok(Some(scarab_identity::Session {
            id: id.to_string(),
            principal,
            csrf_hash: row.get::<String, _>("csrf"),
            expires_at: row.get::<i64, _>("expires_at"),
            created_at: row.get::<i64, _>("created_at"),
            login_kind: scarab_identity::LoginKind::from_stored(
                &row.get::<String, _>("login_kind"),
            ),
        }))
    }

    async fn delete(&self, id: &str) -> Result<(), scarab_identity::IdentityError> {
        sqlx::query("DELETE FROM sessions WHERE id = $1")
            .bind(scarab_identity::session_id_hash(id))
            .execute(self.pool())
            .await
            .map_err(|e| scarab_identity::IdentityError::Issuance(e.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RbacStore (ADR-0049 C2): role bindings in Postgres. project='' encodes an
// org-scoped binding; role NULL is a native revoke tombstone.
// ---------------------------------------------------------------------------

fn scope_cols(scope: &scarab_identity::Scope) -> (&str, &str) {
    match scope {
        scarab_identity::Scope::Org(org) => (org.as_str(), ""),
        scarab_identity::Scope::Project { org, name } => (org.as_str(), name.as_str()),
    }
}

pub(crate) fn role_str(role: scarab_identity::Role) -> &'static str {
    match role {
        scarab_identity::Role::Viewer => "viewer",
        scarab_identity::Role::Member => "member",
        scarab_identity::Role::Admin => "admin",
        scarab_identity::Role::Owner => "owner",
    }
}

pub(crate) fn role_from_str(s: &str) -> Option<scarab_identity::Role> {
    Some(match s {
        "viewer" => scarab_identity::Role::Viewer,
        "member" => scarab_identity::Role::Member,
        "admin" => scarab_identity::Role::Admin,
        "owner" => scarab_identity::Role::Owner,
        _ => return None,
    })
}

pub(crate) fn identity_err(e: sqlx::Error) -> scarab_identity::IdentityError {
    scarab_identity::IdentityError::Issuance(e.to_string())
}

#[async_trait]
impl scarab_identity::RbacStore for PostgresDb {
    async fn grant(
        &self,
        binding: &scarab_identity::Binding,
        origin: scarab_identity::BindingOrigin,
    ) -> Result<(), scarab_identity::IdentityError> {
        let (org, project) = scope_cols(&binding.scope);
        match origin {
            // Native is authoritative: unconditional upsert (also clears a
            // tombstone by writing a real role over it).
            scarab_identity::BindingOrigin::Native => {
                sqlx::query(
                    "INSERT INTO rbac_bindings (subject, org, project, role, origin)
                     VALUES ($1, $2, $3, $4, 'native')
                     ON CONFLICT (subject, org, project)
                     DO UPDATE SET role = EXCLUDED.role, origin = 'native'",
                )
                .bind(&binding.subject)
                .bind(org)
                .bind(project)
                .bind(role_str(binding.role))
                .execute(self.pool())
                .await
                .map_err(identity_err)?;
            }
            // An import only seeds/refreshes rows it owns — a native grant or
            // a native revoke tombstone is NEVER clobbered by a re-sync.
            scarab_identity::BindingOrigin::Import => {
                sqlx::query(
                    "INSERT INTO rbac_bindings (subject, org, project, role, origin)
                     VALUES ($1, $2, $3, $4, 'import')
                     ON CONFLICT (subject, org, project)
                     DO UPDATE SET role = EXCLUDED.role
                     WHERE rbac_bindings.origin = 'import'",
                )
                .bind(&binding.subject)
                .bind(org)
                .bind(project)
                .bind(role_str(binding.role))
                .execute(self.pool())
                .await
                .map_err(identity_err)?;
            }
        }
        Ok(())
    }

    async fn revoke(
        &self,
        subject: &str,
        scope: &scarab_identity::Scope,
    ) -> Result<(), scarab_identity::IdentityError> {
        let (org, project) = scope_cols(scope);
        sqlx::query(
            "INSERT INTO rbac_bindings (subject, org, project, role, origin)
             VALUES ($1, $2, $3, NULL, 'native')
             ON CONFLICT (subject, org, project)
             DO UPDATE SET role = NULL, origin = 'native'",
        )
        .bind(subject)
        .bind(org)
        .bind(project)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(())
    }

    async fn role_of(
        &self,
        subject: &str,
        scope: &scarab_identity::Scope,
    ) -> Result<Option<scarab_identity::Role>, scarab_identity::IdentityError> {
        let (org, project) = scope_cols(scope);
        // Exact scope + the enclosing org (Org role inherits down, ADR-0049);
        // tombstones (role NULL) grant nothing.
        let rows = sqlx::query(
            "SELECT role FROM rbac_bindings
             WHERE subject = $1 AND org = $2 AND (project = $3 OR project = '')
               AND role IS NOT NULL",
        )
        .bind(subject)
        .bind(org)
        .bind(project)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|r| role_from_str(&r.get::<String, _>("role")))
            .max())
    }

    async fn max_role(
        &self,
        subject: &str,
    ) -> Result<Option<scarab_identity::Role>, scarab_identity::IdentityError> {
        // Every scope, tombstones excluded. The max is taken here, on the
        // typed role, not in SQL: the column is text and its order is not
        // the role order. An unreadable role grants nothing.
        let rows =
            sqlx::query("SELECT role FROM rbac_bindings WHERE subject = $1 AND role IS NOT NULL")
                .bind(subject)
                .fetch_all(self.pool())
                .await
                .map_err(identity_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|r| role_from_str(&r.get::<String, _>("role")))
            .max())
    }

    async fn bindings(
        &self,
        org: &str,
    ) -> Result<
        Vec<(scarab_identity::Binding, scarab_identity::BindingOrigin)>,
        scarab_identity::IdentityError,
    > {
        let rows = sqlx::query(
            "SELECT subject, project, role, origin FROM rbac_bindings
             WHERE org = $1 AND role IS NOT NULL
             ORDER BY subject, project",
        )
        .bind(org)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let role = role_from_str(&r.get::<String, _>("role"))?;
                let project = r.get::<String, _>("project");
                let scope = if project.is_empty() {
                    scarab_identity::Scope::Org(org.to_string())
                } else {
                    scarab_identity::Scope::Project {
                        org: org.to_string(),
                        name: project,
                    }
                };
                let origin = match r.get::<String, _>("origin").as_str() {
                    "import" => scarab_identity::BindingOrigin::Import,
                    _ => scarab_identity::BindingOrigin::Native,
                };
                Some((
                    scarab_identity::Binding {
                        subject: r.get::<String, _>("subject"),
                        scope,
                        role,
                    },
                    origin,
                ))
            })
            .collect())
    }

    async fn orgs_of(&self, subject: &str) -> Result<Vec<String>, scarab_identity::IdentityError> {
        // Tombstones (role NULL) are no membership.
        sqlx::query_scalar(
            "SELECT DISTINCT org FROM rbac_bindings
             WHERE subject = $1 AND role IS NOT NULL ORDER BY org",
        )
        .bind(subject)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)
    }

    async fn import_seeds(
        &self,
        scope: &scarab_identity::Scope,
        via: &str,
        seeds: &[(String, scarab_identity::Role)],
        prune: bool,
    ) -> Result<u64, scarab_identity::IdentityError> {
        let (org, project) = scope_cols(scope);
        let mut tx = self.pool().begin().await.map_err(identity_err)?;
        for (subject, role) in seeds {
            // `origin = 'import'` guard: a native grant or tombstone is never
            // touched, exactly as `grant(.., Import)`.
            sqlx::query(
                "INSERT INTO rbac_bindings (subject, org, project, role, origin, imported_via)
                 VALUES ($1, $2, $3, $4, 'import', $5)
                 ON CONFLICT (subject, org, project)
                 DO UPDATE SET role = EXCLUDED.role, imported_via = EXCLUDED.imported_via
                 WHERE rbac_bindings.origin = 'import'",
            )
            .bind(subject)
            .bind(org)
            .bind(project)
            .bind(role_str(*role))
            .bind(via)
            .execute(&mut *tx)
            .await
            .map_err(identity_err)?;
        }
        let mut pruned = 0;
        if prune {
            let keep: Vec<&str> = seeds.iter().map(|(s, _)| s.as_str()).collect();
            pruned = sqlx::query(
                "DELETE FROM rbac_bindings
                 WHERE org = $1 AND project = $2 AND origin = 'import'
                   AND (imported_via = $3 OR imported_via IS NULL)
                   AND NOT (subject = ANY($4))",
            )
            .bind(org)
            .bind(project)
            .bind(via)
            .bind(&keep)
            .execute(&mut *tx)
            .await
            .map_err(identity_err)?
            .rows_affected();
        }
        tx.commit().await.map_err(identity_err)?;
        Ok(pruned)
    }

    async fn drop_imports(
        &self,
        subject: &str,
        via: &str,
    ) -> Result<u64, scarab_identity::IdentityError> {
        Ok(sqlx::query(
            "DELETE FROM rbac_bindings
             WHERE subject = $1 AND origin = 'import' AND imported_via = $2",
        )
        .bind(subject)
        .bind(via)
        .execute(self.pool())
        .await
        .map_err(identity_err)?
        .rows_affected())
    }
}

// ---------------------------------------------------------------------------
// ApiTokenStore (ADR-0049 amendment): issued API tokens in Postgres. The row
// holds the SHA-256 of the plaintext and never the plaintext; project=''
// encodes an org-scoped token, exactly as it does for a binding.
// ---------------------------------------------------------------------------

fn api_token_from_row(r: &sqlx::postgres::PgRow) -> Option<scarab_identity::ApiToken> {
    let project = r.get::<String, _>("scope_project");
    let org = r.get::<String, _>("scope_org");
    let scope = if project.is_empty() {
        scarab_identity::Scope::Org(org)
    } else {
        scarab_identity::Scope::Project { org, name: project }
    };
    Some(scarab_identity::ApiToken {
        id: r.get::<String, _>("id"),
        name: r.get::<String, _>("name"),
        owner_subject: r.get::<String, _>("owner_subject"),
        scope,
        // An unreadable role fails CLOSED: the row is dropped rather than
        // defaulted, so a future role this binary does not know cannot be
        // silently downgraded into one it does.
        role: role_from_str(&r.get::<String, _>("role"))?,
        expires_at: r.get::<i64, _>("expires_at"),
        created_by: r.get::<String, _>("created_by"),
        created_at: r.get::<i64, _>("created_at"),
        last_used_at: r.get::<Option<i64>, _>("last_used_at"),
        // BIGINT, never negative: it only ever grows by `touch`'s `uses`.
        use_count: u64::try_from(r.get::<i64, _>("use_count")).unwrap_or(0),
        revoked_at: r.get::<Option<i64>, _>("revoked_at"),
    })
}

/// How many `~N` suffixes a mint will try before giving up. A real deployment
/// needs a handful; this bound only stops a pathological loop.
const MAX_PRINCIPAL_SUFFIX: u32 = 10_000;

/// The columns [`principal_from_row`] reads.
const PRINCIPAL_COLS: &str = "id, handle, display_name, created_at, claimed_at, merged_into";

fn principal_from_row(r: &sqlx::postgres::PgRow) -> scarab_identity::PrincipalRecord {
    scarab_identity::PrincipalRecord {
        id: r.get("id"),
        handle: r.get("handle"),
        display_name: r.get("display_name"),
        created_at: r.get("created_at"),
        claimed_at: r.get("claimed_at"),
        merged_into: r.get("merged_into"),
    }
}

/// Fold Principal `from` into `into` inside `tx` (ADR-0069 §6). Everything
/// that NAMES `from` and would otherwise strand its authority moves:
///
/// - **bindings**, scope by scope: `into`'s own native row (grant or revoke)
///   stays; else `from`'s native row moves, tombstones included; between two
///   import rows the higher role wins. Native always beats import, whatever
///   the rank — a deliberate native downgrade is never undone by a merge;
/// - **approver entries** in every environment of every org;
/// - **API tokens** (`owner_subject`), so they stay capped by `into`'s roles;
/// - **passkeys** of `from` are deleted, not moved (ADR-0079 §3): a passkey
///   is registered for one Principal. Their ids are returned so the caller
///   can record each removal;
/// - **sessions** of `from` are deleted;
/// - `from` itself is marked `merged_into` and kept, so its id is never minted
///   again. History (event `by`, `approved_by`) is not rewritten.
async fn merge_principal(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    from: &str,
    into: &str,
) -> Result<Vec<String>, scarab_identity::IdentityError> {
    let rows = sqlx::query(
        "SELECT org, project, role, origin, imported_via FROM rbac_bindings
         WHERE subject = $1 FOR UPDATE",
    )
    .bind(from)
    .fetch_all(&mut **tx)
    .await
    .map_err(identity_err)?;
    for r in rows {
        let org: String = r.get("org");
        let project: String = r.get("project");
        let role: Option<String> = r.get("role");
        let origin: String = r.get("origin");
        let via: Option<String> = r.get("imported_via");
        let theirs = sqlx::query(
            "SELECT role, origin FROM rbac_bindings
             WHERE subject = $1 AND org = $2 AND project = $3 FOR UPDATE",
        )
        .bind(into)
        .bind(&org)
        .bind(&project)
        .fetch_optional(&mut **tx)
        .await
        .map_err(identity_err)?;
        let keep_from = match theirs {
            None => true,
            Some(t) => {
                let t_origin: String = t.get("origin");
                let t_role: Option<String> = t.get("role");
                if t_origin == "native" {
                    false
                } else if origin == "native" {
                    true
                } else {
                    // Two import rows: the higher role wins.
                    let rank = |r: &Option<String>| r.as_deref().and_then(role_from_str);
                    rank(&role) > rank(&t_role)
                }
            }
        };
        if keep_from {
            sqlx::query(
                "DELETE FROM rbac_bindings WHERE subject = $1 AND org = $2 AND project = $3",
            )
            .bind(into)
            .bind(&org)
            .bind(&project)
            .execute(&mut **tx)
            .await
            .map_err(identity_err)?;
            sqlx::query(
                "INSERT INTO rbac_bindings (subject, org, project, role, origin, imported_via)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(into)
            .bind(&org)
            .bind(&project)
            .bind(&role)
            .bind(&origin)
            .bind(&via)
            .execute(&mut **tx)
            .await
            .map_err(identity_err)?;
        }
        sqlx::query("DELETE FROM rbac_bindings WHERE subject = $1 AND org = $2 AND project = $3")
            .bind(from)
            .bind(&org)
            .bind(&project)
            .execute(&mut **tx)
            .await
            .map_err(identity_err)?;
    }

    // Approver lists name Principals by exact id (ADR-0069 §3), so a folded id
    // is rewritten wherever admission reads approvers — the org Environment
    // baselines and the Project overlays (ADR-0071 §1, §3) — order kept, the
    // survivor not listed twice.
    for table in ["org_environments", "environment_project_overlays"] {
        sqlx::query(&format!(
            "UPDATE {table} SET approvers = (
                 SELECT COALESCE(array_agg(a ORDER BY first_seen), '{{}}')
                 FROM (SELECT a, min(i) AS first_seen
                       FROM unnest(array_replace(approvers, $1, $2)) WITH ORDINALITY u(a, i)
                       GROUP BY a) folded)
             WHERE $1 = ANY(approvers)"
        ))
        .bind(from)
        .bind(into)
        .execute(&mut **tx)
        .await
        .map_err(identity_err)?;
    }
    // A passkey is registered for one Principal and never moves (ADR-0079
    // §3): `from`'s are deleted with it. The ids are the rows this DELETE
    // removed, so a passkey enrolled for `from` while the link ran is
    // either removed and counted, or left on a Principal nobody signs in as.
    let mut removed_passkeys: Vec<String> =
        sqlx::query_scalar("DELETE FROM passkeys WHERE principal_id = $1 RETURNING id")
            .bind(from)
            .fetch_all(&mut **tx)
            .await
            .map_err(identity_err)?;
    removed_passkeys.sort();
    sqlx::query("UPDATE api_tokens SET owner_subject = $2 WHERE owner_subject = $1")
        .bind(from)
        .bind(into)
        .execute(&mut **tx)
        .await
        .map_err(identity_err)?;
    sqlx::query("DELETE FROM sessions WHERE principal->>'subject' = $1")
        .bind(from)
        .execute(&mut **tx)
        .await
        .map_err(identity_err)?;
    sqlx::query("UPDATE principals SET merged_into = $2 WHERE id = $1 OR merged_into = $1")
        .bind(from)
        .bind(into)
        .execute(&mut **tx)
        .await
        .map_err(identity_err)?;
    Ok(removed_passkeys)
}

impl PostgresDb {
    /// The identity's Principal, or a freshly minted one (a new random id)
    /// under the first free `~N` of `base_handle`, compared ignoring case
    /// (ADR-0069 §2, ADR-0080 §1). `claim` = a sign-in: refresh the contact
    /// details and stamp `claimed_at`. Otherwise an import: refresh the
    /// handle only, and mint unclaimed.
    async fn pin_or_mint(
        &self,
        identity: &scarab_identity::ProviderIdentity,
        base_handle: &str,
        placeholder: scarab_identity::Placeholder,
        now_ms: i64,
        claim: bool,
    ) -> Result<scarab_identity::PrincipalRecord, scarab_identity::IdentityError> {
        // Two passes at most: if a racing sign-in pins the same identity
        // between our lookup and our insert, our mint rolls back and the
        // second lookup finds theirs. Converging on ONE Principal is the point.
        for _ in 0..2 {
            let pinned: Option<String> = sqlx::query_scalar(
                "SELECT principal_id FROM principal_identities
                 WHERE provider = $1 AND external_id = $2",
            )
            .bind(&identity.provider)
            .bind(&identity.external_id)
            .fetch_optional(self.pool())
            .await
            .map_err(identity_err)?;
            if let Some(id) = pinned {
                if !claim {
                    sqlx::query(
                        "UPDATE principal_identities SET handle = $3
                         WHERE provider = $1 AND external_id = $2",
                    )
                    .bind(&identity.provider)
                    .bind(&identity.external_id)
                    .bind(&identity.handle)
                    .execute(self.pool())
                    .await
                    .map_err(identity_err)?;
                } else {
                    sqlx::query(
                        "UPDATE principal_identities
                         SET handle = $3, email = $4, email_verified = $5, last_login_at = $6
                         WHERE provider = $1 AND external_id = $2",
                    )
                    .bind(&identity.provider)
                    .bind(&identity.external_id)
                    .bind(&identity.handle)
                    .bind(&identity.email)
                    .bind(identity.email_verified)
                    .bind(now_ms)
                    .execute(self.pool())
                    .await
                    .map_err(identity_err)?;
                    sqlx::query(
                        "UPDATE principals
                         SET display_name = COALESCE($2, display_name),
                             claimed_at = COALESCE(claimed_at, $3)
                         WHERE id = $1",
                    )
                    .bind(&id)
                    .bind(&identity.display_name)
                    .bind(now_ms)
                    .execute(self.pool())
                    .await
                    .map_err(identity_err)?;
                }
                return self.principal_required(&id).await;
            }

            let mut tx = self.pool().begin().await.map_err(identity_err)?;
            // A legacy placeholder (migration 0093) whose handle is exactly
            // this one: adopted, never minted around, so a pre-0052 grant
            // reaches the person the pre-0052 server would have given it to.
            let mut minted: Option<String> = None;
            if placeholder == scarab_identity::Placeholder::AdoptExact {
                minted = sqlx::query_scalar(
                    "UPDATE principals
                     SET legacy_placeholder = FALSE,
                         display_name = COALESCE($2, display_name),
                         claimed_at = $3
                     WHERE id = (SELECT id FROM principals
                                 WHERE handle = $1 AND legacy_placeholder
                                   AND claimed_at IS NULL AND merged_into IS NULL
                                 FOR UPDATE)
                     RETURNING id",
                )
                .bind(base_handle)
                .bind(&identity.display_name)
                .bind(claim.then_some(now_ms))
                .fetch_optional(&mut *tx)
                .await
                .map_err(identity_err)?;
            }
            // Never seen: mint. The unique indexes (the handle, and the
            // handle ignoring case) are the existence check — insert and, on
            // conflict, take the next suffix. No check-then-act.
            if minted.is_none() {
                for n in 1..=MAX_PRINCIPAL_SUFFIX {
                    let handle = scarab_identity::handle_candidate(base_handle, n);
                    let id: Option<String> = sqlx::query_scalar(
                        "INSERT INTO principals (handle, display_name, created_at, claimed_at)
                         VALUES ($1, $2, $3, $4)
                         ON CONFLICT DO NOTHING
                         RETURNING id",
                    )
                    .bind(&handle)
                    .bind(&identity.display_name)
                    .bind(now_ms)
                    .bind(claim.then_some(now_ms))
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(identity_err)?;
                    if id.is_some() {
                        minted = id;
                        break;
                    }
                }
            }
            let id = minted.ok_or_else(|| {
                scarab_identity::IdentityError::Issuance(format!(
                    "no free principal handle for `{base_handle}`"
                ))
            })?;
            let pinned_now = sqlx::query(
                "INSERT INTO principal_identities
                     (provider, external_id, principal_id, handle, email, email_verified,
                      linked_at, last_login_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (provider, external_id) DO NOTHING",
            )
            .bind(&identity.provider)
            .bind(&identity.external_id)
            .bind(&id)
            .bind(&identity.handle)
            .bind(&identity.email)
            .bind(identity.email_verified)
            .bind(now_ms)
            .bind(claim.then_some(now_ms))
            .execute(&mut *tx)
            .await
            .map_err(identity_err)?
            .rows_affected();
            if pinned_now == 1 {
                tx.commit().await.map_err(identity_err)?;
                return self.principal_required(&id).await;
            }
            // Lost the race: drop our unseen mint and resolve theirs.
            tx.rollback().await.map_err(identity_err)?;
        }
        Err(scarab_identity::IdentityError::Issuance(format!(
            "identity {}:{} could not be pinned",
            identity.provider, identity.external_id
        )))
    }

    async fn principal_required(
        &self,
        id: &str,
    ) -> Result<scarab_identity::PrincipalRecord, scarab_identity::IdentityError> {
        scarab_identity::IdentityStore::principal(self, id)
            .await?
            .ok_or_else(|| {
                scarab_identity::IdentityError::Issuance(format!("principal `{id}` vanished"))
            })
    }

    /// The handle of Principal `id`, or `id` itself when it names nobody.
    async fn handle_of(&self, tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT handle FROM principals WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| id.to_string())
    }

    /// The handles that equal another ignoring case (ADR-0080 §6), grouped:
    /// each inner list is one group, oldest first. Logged at boot so an
    /// operator can see them; nothing merges them automatically.
    pub async fn case_duplicate_handles(
        &self,
    ) -> Result<Vec<Vec<String>>, scarab_identity::IdentityError> {
        let rows = sqlx::query(
            "SELECT array_agg(handle ORDER BY created_at, handle) AS handles
             FROM principals
             GROUP BY lower(handle)
             HAVING count(*) > 1
             ORDER BY lower(handle)",
        )
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows
            .iter()
            .map(|r| r.get::<Vec<String>, _>("handles"))
            .collect())
    }
}

#[async_trait]
impl scarab_identity::IdentityStore for PostgresDb {
    async fn pinned(
        &self,
        provider: &str,
        external_id: &str,
    ) -> Result<Option<String>, scarab_identity::IdentityError> {
        sqlx::query_scalar(
            "SELECT principal_id FROM principal_identities
             WHERE provider = $1 AND external_id = $2",
        )
        .bind(provider)
        .bind(external_id)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)
    }

    async fn sign_in(
        &self,
        identity: &scarab_identity::ProviderIdentity,
        base_handle: &str,
        placeholder: scarab_identity::Placeholder,
        now_ms: i64,
    ) -> Result<scarab_identity::PrincipalRecord, scarab_identity::IdentityError> {
        self.pin_or_mint(identity, base_handle, placeholder, now_ms, true)
            .await
    }

    async fn pin(
        &self,
        identity: &scarab_identity::ProviderIdentity,
        base_handle: &str,
        placeholder: scarab_identity::Placeholder,
        now_ms: i64,
    ) -> Result<scarab_identity::PrincipalRecord, scarab_identity::IdentityError> {
        self.pin_or_mint(identity, base_handle, placeholder, now_ms, false)
            .await
    }

    async fn link(
        &self,
        principal: &str,
        identity: &scarab_identity::ProviderIdentity,
        now_ms: i64,
        folded: &scarab_identity::FoldedPasskey,
    ) -> Result<scarab_identity::LinkOutcome, scarab_identity::IdentityError> {
        let conflict = |m: String| scarab_identity::IdentityError::Conflict(m);
        let mut tx = self.pool().begin().await.map_err(identity_err)?;
        // Lock the identity row (if any) so a racing sign-in or link of the
        // same account serializes behind this decision.
        let holder: Option<String> = sqlx::query_scalar(
            "SELECT principal_id FROM principal_identities
             WHERE provider = $1 AND external_id = $2 FOR UPDATE",
        )
        .bind(&identity.provider)
        .bind(&identity.external_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(identity_err)?;
        if holder.as_deref() == Some(principal) {
            return Ok(scarab_identity::LinkOutcome::AlreadyLinked);
        }
        let mine: Option<String> = sqlx::query_scalar(
            "SELECT handle FROM principal_identities WHERE principal_id = $1 AND provider = $2",
        )
        .bind(principal)
        .bind(&identity.provider)
        .fetch_optional(&mut *tx)
        .await
        .map_err(identity_err)?;
        if let Some(handle) = mine {
            return Err(conflict(format!(
                "you already have a {} account linked (`{handle}`) — unlink it first",
                identity.provider
            )));
        }
        let outcome = match holder {
            None => {
                sqlx::query(
                    "INSERT INTO principal_identities
                         (provider, external_id, principal_id, handle, email, email_verified,
                          linked_at, last_login_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $7)",
                )
                .bind(&identity.provider)
                .bind(&identity.external_id)
                .bind(principal)
                .bind(&identity.handle)
                .bind(&identity.email)
                .bind(identity.email_verified)
                .bind(now_ms)
                .execute(&mut *tx)
                .await
                .map_err(identity_err)?;
                scarab_identity::LinkOutcome::Attached
            }
            Some(from) => {
                let held: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM principal_identities WHERE principal_id = $1",
                )
                .bind(&from)
                .fetch_one(&mut *tx)
                .await
                .map_err(identity_err)?;
                let from_handle = self.handle_of(&mut tx, &from).await;
                if held > 1 {
                    return Err(conflict(format!(
                        "that {} account belongs to `{from_handle}`, which also signs in \
                         another way — proving this one account does not prove you are \
                         `{from_handle}`, so merging the two needs an admin",
                        identity.provider
                    )));
                }
                // ADR-0079 §2: folding in a Principal that holds any passkey
                // (a flagged one too) needs an assertion from one of them.
                // A row lock does not hold back an INSERT, so a passkey
                // enrolled for `from` after this read is caught below, where
                // the merge's DELETE sees it.
                let held_passkeys: Vec<String> = sqlx::query_scalar(
                    "SELECT id FROM passkeys WHERE principal_id = $1 FOR UPDATE",
                )
                .bind(&from)
                .fetch_all(&mut *tx)
                .await
                .map_err(identity_err)?;
                let proven = match folded {
                    scarab_identity::FoldedPasskey::Proven(id) => held_passkeys.contains(id),
                    scarab_identity::FoldedPasskey::Unproven => false,
                };
                if !held_passkeys.is_empty() && !proven {
                    tx.rollback().await.map_err(identity_err)?;
                    return Ok(scarab_identity::LinkOutcome::NeedsFoldedPasskey {
                        from,
                        from_handle,
                    });
                }
                let removed_passkeys = merge_principal(&mut tx, &from, principal).await?;
                // The DELETE removed a passkey the check did not see (enrolled
                // in between): the decision above was made without it, so
                // nothing changes (git-bug 1a81c63).
                if removed_passkeys
                    .iter()
                    .any(|id| !held_passkeys.contains(id))
                {
                    tx.rollback().await.map_err(identity_err)?;
                    return Ok(scarab_identity::LinkOutcome::NeedsFoldedPasskey {
                        from,
                        from_handle,
                    });
                }
                sqlx::query(
                    "UPDATE principal_identities
                     SET principal_id = $3, handle = $4, email = $5, email_verified = $6,
                         last_login_at = $7
                     WHERE provider = $1 AND external_id = $2",
                )
                .bind(&identity.provider)
                .bind(&identity.external_id)
                .bind(principal)
                .bind(&identity.handle)
                .bind(&identity.email)
                .bind(identity.email_verified)
                .bind(now_ms)
                .execute(&mut *tx)
                .await
                .map_err(identity_err)?;
                scarab_identity::LinkOutcome::Merged {
                    from,
                    from_handle,
                    removed_passkeys,
                }
            }
        };
        tx.commit().await.map_err(identity_err)?;
        Ok(outcome)
    }

    async fn unlink(
        &self,
        principal: &str,
        provider: &str,
    ) -> Result<(), scarab_identity::IdentityError> {
        let mut tx = self.pool().begin().await.map_err(identity_err)?;
        let providers: Vec<String> = sqlx::query_scalar(
            "SELECT provider FROM principal_identities WHERE principal_id = $1 FOR UPDATE",
        )
        .bind(principal)
        .fetch_all(&mut *tx)
        .await
        .map_err(identity_err)?;
        if !providers.iter().any(|p| p == provider) {
            return Err(scarab_identity::IdentityError::Conflict(format!(
                "no {provider} account is linked"
            )));
        }
        if providers.len() == 1 {
            return Err(scarab_identity::IdentityError::Conflict(format!(
                "{provider} is your only way to sign in — link another account first"
            )));
        }
        sqlx::query("DELETE FROM principal_identities WHERE principal_id = $1 AND provider = $2")
            .bind(principal)
            .bind(provider)
            .execute(&mut *tx)
            .await
            .map_err(identity_err)?;
        tx.commit().await.map_err(identity_err)?;
        Ok(())
    }

    async fn aliases_of(
        &self,
        id: &str,
    ) -> Result<Vec<scarab_identity::PrincipalRecord>, scarab_identity::IdentityError> {
        let rows = sqlx::query(&format!(
            "WITH RECURSIVE folded(id) AS (
                 SELECT id FROM principals WHERE merged_into = $1
                 UNION
                 SELECT p.id FROM principals p JOIN folded f ON p.merged_into = f.id
             )
             SELECT {PRINCIPAL_COLS} FROM principals WHERE id IN (SELECT id FROM folded)
             ORDER BY handle"
        ))
        .bind(id)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows.iter().map(principal_from_row).collect())
    }

    async fn principal(
        &self,
        id: &str,
    ) -> Result<Option<scarab_identity::PrincipalRecord>, scarab_identity::IdentityError> {
        let row = sqlx::query(&format!(
            "SELECT {PRINCIPAL_COLS} FROM principals WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(row.as_ref().map(principal_from_row))
    }

    async fn by_handle(
        &self,
        handle: &str,
    ) -> Result<Vec<scarab_identity::PrincipalRecord>, scarab_identity::IdentityError> {
        let exact = sqlx::query(&format!(
            "SELECT {PRINCIPAL_COLS} FROM principals WHERE handle = $1"
        ))
        .bind(handle)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        if let Some(r) = exact {
            return Ok(vec![principal_from_row(&r)]);
        }
        let rows = sqlx::query(&format!(
            "SELECT {PRINCIPAL_COLS} FROM principals WHERE lower(handle) = lower($1)
             ORDER BY handle"
        ))
        .bind(handle)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows.iter().map(principal_from_row).collect())
    }

    async fn lookup_many(
        &self,
        refs: &[String],
    ) -> Result<Vec<scarab_identity::PrincipalRecord>, scarab_identity::IdentityError> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(&format!(
            "SELECT {PRINCIPAL_COLS} FROM principals WHERE id = ANY($1) OR handle = ANY($1)"
        ))
        .bind(refs)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows.iter().map(principal_from_row).collect())
    }

    async fn identities_of(
        &self,
        id: &str,
    ) -> Result<Vec<scarab_identity::ProviderIdentity>, scarab_identity::IdentityError> {
        let rows = sqlx::query(
            "SELECT i.provider, i.external_id, i.handle, i.email, i.email_verified,
                    p.display_name
             FROM principal_identities i JOIN principals p ON p.id = i.principal_id
             WHERE i.principal_id = $1
             ORDER BY i.provider",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows
            .into_iter()
            .map(|r| scarab_identity::ProviderIdentity {
                provider: r.get("provider"),
                external_id: r.get("external_id"),
                handle: r.get("handle"),
                email: r.get("email"),
                email_verified: r.get("email_verified"),
                display_name: r.get("display_name"),
            })
            .collect())
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<scarab_identity::PrincipalMatch>, scarab_identity::IdentityError> {
        let q = scarab_identity::like_literal(query);
        let rows = sqlx::query(
            "SELECT p.id, p.handle, p.display_name, p.claimed_at FROM principals p
             WHERE p.merged_into IS NULL
               AND (p.handle ILIKE $1 || '%'
                    OR p.display_name ILIKE '%' || $1 || '%'
                    OR EXISTS (SELECT 1 FROM principal_identities i
                               WHERE i.principal_id = p.id AND i.handle ILIKE $1 || '%'))
             ORDER BY p.handle
             LIMIT $2",
        )
        .bind(&q)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: String = r.get("id");
            out.push(scarab_identity::PrincipalMatch {
                identities: self.identities_of(&id).await?,
                handle: r.get("handle"),
                display_name: r.get("display_name"),
                claimed: r.get::<Option<i64>, _>("claimed_at").is_some(),
                id,
            });
        }
        Ok(out)
    }
}

const API_TOKEN_COLS: &str = "id, name, owner_subject, scope_org, scope_project, role, \
                              expires_at, created_by, created_at, last_used_at, use_count, \
                              revoked_at";

#[async_trait]
impl scarab_identity::ApiTokenStore for PostgresDb {
    async fn put(
        &self,
        token: &scarab_identity::ApiToken,
        hash: &str,
    ) -> Result<(), scarab_identity::IdentityError> {
        let (org, project) = scope_cols(&token.scope);
        // No upsert: a token id is minted fresh and a hash is unique by
        // construction, so a conflict here means a CSPRNG collision or a bug —
        // either way the right answer is to fail, not to overwrite a live
        // credential's row with a different one.
        sqlx::query(
            "INSERT INTO api_tokens
                 (id, token_hash, name, owner_subject, scope_org, scope_project,
                  role, expires_at, created_by, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(&token.id)
        .bind(hash)
        .bind(&token.name)
        .bind(&token.owner_subject)
        .bind(org)
        .bind(project)
        .bind(role_str(token.role))
        .bind(token.expires_at)
        .bind(&token.created_by)
        .bind(token.created_at)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(())
    }

    async fn by_hash(
        &self,
        hash: &str,
    ) -> Result<Option<scarab_identity::ApiToken>, scarab_identity::IdentityError> {
        let row = sqlx::query(&format!(
            "SELECT {API_TOKEN_COLS} FROM api_tokens WHERE token_hash = $1"
        ))
        .bind(hash)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(row.as_ref().and_then(api_token_from_row))
    }

    async fn by_id(
        &self,
        id: &str,
    ) -> Result<Option<scarab_identity::ApiToken>, scarab_identity::IdentityError> {
        let row = sqlx::query(&format!(
            "SELECT {API_TOKEN_COLS} FROM api_tokens WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(row.as_ref().and_then(api_token_from_row))
    }

    async fn list(
        &self,
        org: &str,
    ) -> Result<Vec<scarab_identity::ApiToken>, scarab_identity::IdentityError> {
        let rows = sqlx::query(&format!(
            "SELECT {API_TOKEN_COLS} FROM api_tokens WHERE scope_org = $1 \
             ORDER BY created_at DESC, id"
        ))
        .bind(org)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows.iter().filter_map(api_token_from_row).collect())
    }

    async fn list_owned(
        &self,
        owner: &str,
    ) -> Result<Vec<scarab_identity::ApiToken>, scarab_identity::IdentityError> {
        // `api_tokens_owner_idx` serves this.
        let rows = sqlx::query(&format!(
            "SELECT {API_TOKEN_COLS} FROM api_tokens WHERE owner_subject = $1 \
             ORDER BY created_at DESC, id"
        ))
        .bind(owner)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows.iter().filter_map(api_token_from_row).collect())
    }

    async fn revoke(&self, id: &str, now_ms: i64) -> Result<bool, scarab_identity::IdentityError> {
        // `revoked_at IS NULL` makes this idempotent AND honest: a second
        // revoke reports false rather than moving the stamp, so the record of
        // when the credential actually died is never rewritten.
        let done = sqlx::query(
            "UPDATE api_tokens SET revoked_at = $2 WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(now_ms)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(done.rows_affected() > 0)
    }

    async fn touch(
        &self,
        id: &str,
        now_ms: i64,
        uses: u64,
    ) -> Result<(), scarab_identity::IdentityError> {
        // Monotonic: two in-flight requests cannot make last_used_at go
        // backwards, which would misreport a token as staler than it is. The
        // count is added in the same statement, so a late batch still counts.
        sqlx::query(
            "UPDATE api_tokens
                SET last_used_at = GREATEST(COALESCE(last_used_at, $2), $2),
                    use_count = use_count + $3
              WHERE id = $1",
        )
        .bind(id)
        .bind(now_ms)
        .bind(i64::try_from(uses).unwrap_or(i64::MAX))
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(())
    }
}
