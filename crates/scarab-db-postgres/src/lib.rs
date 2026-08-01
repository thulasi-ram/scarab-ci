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

use scarab_engine::ports::Lease;
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, ConcurrencyPolicy, Db, DbError, EventKind, EventPayload,
    FailureKind, LogChunkMeta, OutboxId, OutboxMessage, RunId, RunService, RunStatus, RunSummary,
    ServiceStatus, StepId, StepRun, StepSpec, StepStatus, Timestamp,
};
use scarab_forge::{
    ForgeConnection, ForgeConnectionStore, ForgeKind, RegistryError, RepoRef, ResolvedRepo,
};
use scarab_project::{Deployment, Environment, EnvironmentStore, ProjectError};

/// The embedded, ordered set of forward-only migrations. `MIGRATOR.run(pool)`
/// applies all pending ones (tracked in `_sqlx_migrations`); tests can also walk
/// `MIGRATOR.iter()` to apply schema versions one at a time.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// A Postgres-backed [`Db`]. Always connected: Postgres is mandatory for
/// every serving role (ADR-0048) — the unconnected/API-only construction was
/// deleted, not guarded.
pub struct PostgresDb {
    pool: PgPool,
}

impl PostgresDb {
    /// Construct from an existing connection pool.
    pub fn with_pool(pool: PgPool) -> Self {
        Self { pool }
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

    /// All attempts of a step, in start order.
    ///
    /// Deterministic tiebreak on the attempt id: `started_at` alone is not
    /// unique (the test `FakeClock` ties, and fast real execution can mint two
    /// attempts in the same millisecond), which would make attempt order — and
    /// therefore `.last()`, the frontier that anchors `?attempt=` reads and the
    /// settle-path frontier guard — nondeterministic. Attempt ids are minted
    /// `a{n}` (`a1`,`a2`,…; monotonic — see the scheduler), so ties break on the
    /// numeric suffix. It must be numeric, not lexical: lexical order puts `a10`
    /// before `a2`. The in-memory `Db` (scarab-testkit) mirrors this exact order.
    pub async fn attempts(&self, run: &RunId, step: &StepId) -> Result<Vec<Attempt>, DbError> {
        let rows = sqlx::query(
            "SELECT attempt_id, started_at, failure, failure_detail, output_durability, outcome
             FROM attempts
             WHERE run_id = $1 AND step_id = $2
             ORDER BY started_at, CAST(substring(attempt_id FROM 2) AS INTEGER)",
        )
        .bind(&run.0)
        .bind(&step.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.into_iter()
            .map(|r| {
                let failure = r
                    .get::<Option<String>, _>("failure")
                    .map(|s| failure_from_str(&s))
                    .transpose()?;
                // Back-compat derivation lives at this storage boundary — the
                // only place the NULL `outcome` column is observed — so the
                // in-memory `Attempt.outcome` is authoritative for every
                // consumer. A pre-migration row (NULL outcome) derives `Failed`
                // when a failure was recorded, else `Running`.
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
                    // NULL = pre-0064-s2 row / no workspace / stamp-less
                    // backend — absence of evidence, reported as such.
                    output_durability: r.get::<Option<String>, _>("output_durability"),
                    outcome,
                })
            })
            .collect()
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
    async fn claim_ready_steps(&self, limit: u32) -> Result<Vec<StepRun>, DbError> {
        // Atomic dequeue: the inner SELECT locks up to `limit` ready rows with
        // FOR UPDATE SKIP LOCKED (concurrent claimers skip each other's locked
        // rows), and the outer UPDATE flips them to `running` in the same
        // statement. After commit the rows are no longer `ready`, so a later
        // claim cannot hand out the same step — no double-dispatch.
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
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;

        let mut claimed = Vec::with_capacity(rows.len());
        for r in rows {
            let run = RunId(r.get::<String, _>("run_id"));
            let step = StepId(r.get::<String, _>("step_id"));
            let status = step_status_from_str(r.get::<String, _>("status"))?;
            let needs = needs_from_value(r.get::<Value, _>("needs"))?;
            let gate_kind = r.get::<Option<String>, _>("gate_kind");
            let attempts = self.attempts(&run, &step).await?;
            claimed.push(StepRun {
                run,
                step,
                status,
                attempts,
                needs,
                gate_kind,
            });
        }
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

    async fn set_step_output(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        snapshot: &str,
        identity: Option<&str>,
        durability: Option<&str>,
    ) -> Result<(), DbError> {
        // One transaction (ADR-0056): the attempt's immutable evidence copy
        // and the step's latest-evidence denormalization (+ its provenance
        // stamp) move together or not at all. The identity travels in the same
        // statement as the root it describes — a row with one and not the other
        // would be a snapshot whose content nobody can compare (ADR-0061 s8).
        // The durability stamp (ADR-0064 s2) rides the ATTEMPT update only:
        // it is per-attempt historical evidence, never denormalized.
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
             SET output_snapshot = $4, output_identity = $5, output_durability = $6
             WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.0)
        .bind(snapshot)
        .bind(identity)
        .bind(durability)
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

    async fn acquire_slot(&self, group: &str, run: &RunId) -> Result<Option<RunId>, DbError> {
        // Serialize acquirers on this group with a row lock, so exactly one wins.
        let mut tx = self.pool().begin().await.map_err(db_err)?;
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

    async fn create_run_service(
        &self,
        run: &RunId,
        take: i64,
        name: &str,
        at: Timestamp,
    ) -> Result<(), DbError> {
        // Idempotent on {run, take, name} (ADR-0058): a re-tick / crash resume
        // never provisions a second instance.
        sqlx::query(
            "INSERT INTO run_services (run_id, take, name, status, created_at, updated_at)
             VALUES ($1, $2, $3, 'starting', $4, $4)
             ON CONFLICT (run_id, take, name) DO NOTHING",
        )
        .bind(&run.0)
        .bind(take)
        .bind(name)
        .bind(at.0)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn set_run_service(
        &self,
        run: &RunId,
        take: i64,
        name: &str,
        status: ServiceStatus,
        handle: Option<&str>,
    ) -> Result<(), DbError> {
        // COALESCE keeps a previously-recorded handle when this update omits one.
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
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
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

    async fn active_runs(&self) -> Result<Vec<RunId>, DbError> {
        // Priority order (higher first, then oldest) so the admission pass hands
        // scarce capacity to higher-priority work before lower (ADR-0011, 0032).
        let rows = sqlx::query(
            "SELECT id FROM runs WHERE status IN ('pending', 'running', 'suspended')
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
                    origin_pr_base, pipeline, trigger_title
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

    async fn list_runs_for_tenant(
        &self,
        org: &str,
        project: &str,
        limit: u32,
    ) -> Result<Vec<RunSummary>, DbError> {
        let rows = sqlx::query(
            "SELECT id, status, created_at, updated_at, tenant_org, tenant_project, run_number,
                    origin_trigger_kind, origin_actor, origin_ref, origin_sha, origin_pr_number,
                    origin_pr_base, pipeline, trigger_title
             FROM runs
             WHERE tenant_org = $1 AND tenant_project = $2
             ORDER BY created_at DESC, id DESC
             LIMIT $3",
        )
        .bind(org)
        .bind(project)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.into_iter().map(run_summary_from_row).collect()
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
        let affected = sqlx::query(
            "UPDATE step_runs
             SET status = $4, updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND step_id = $2 AND status = $3",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(step_status_str(from))
        .bind(step_status_str(to))
        .execute(self.pool())
        .await
        .map_err(db_err)?
        .rows_affected();
        if affected == 0 {
            return Err(DbError::Conflict);
        }
        Ok(())
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
        sqlx::query(
            "INSERT INTO attempts
                 (run_id, step_id, attempt_id, started_at, failure, failure_detail,
                  output_durability, outcome)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (run_id, step_id, attempt_id) DO NOTHING",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.id.0)
        .bind(attempt.started_at.0)
        .bind(attempt.failure.map(failure_str))
        .bind(attempt.failure_detail.as_deref())
        .bind(attempt.output_durability.as_deref())
        .bind(attempt.outcome.as_str())
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
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
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
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
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
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
        let affected = sqlx::query(
            "UPDATE runs
             SET status = $3, updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1 AND status = $2",
        )
        .bind(&run.0)
        .bind(run_status_str(from))
        .bind(run_status_str(to))
        .execute(self.pool())
        .await
        .map_err(db_err)?
        .rows_affected();
        if affected == 0 {
            return Err(DbError::Conflict);
        }
        Ok(())
    }

    async fn append_event(&self, event: &EventKind) -> Result<(), DbError> {
        let payload =
            serde_json::to_value(&event.kind).map_err(|e| DbError::Other(e.to_string()))?;
        sqlx::query("INSERT INTO events (run_id, version, at, payload) VALUES ($1, $2, $3, $4)")
            .bind(&event.run.0)
            .bind(event.version as i32)
            .bind(event.at.0)
            .bind(payload)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn enqueue_outbox(&self, msg: &OutboxMessage) -> Result<(), DbError> {
        // Unique idempotency_key: a retried enqueue collapses to the existing
        // row, so the logical effect is enqueued exactly once.
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
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
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

    async fn lease(&self, resource: &str, owner: &str, ttl_ms: i64) -> Result<Lease, DbError> {
        // Acquire or renew, taking over only an expired lease. The holder's own
        // re-request is a RENEWAL and must EXTEND `expires_at` (the port
        // contract: the scheduler's `is_leader` re-leases every tick to keep
        // leadership) — hence the `owner = EXCLUDED.owner` arm; without it a
        // constantly-renewing leader still expired after one TTL and a peer
        // could steal leadership at the boundary. A different owner only gets
        // the DO UPDATE once the incumbent lease has expired. RETURNING yields
        // the winning holder; if the incumbent lease is still valid the DO
        // UPDATE is skipped and we read back the current holder instead.
        let row = sqlx::query(
            "INSERT INTO leases (resource, owner, expires_at)
             VALUES ($1, $2, (extract(epoch from now()) * 1000)::bigint + $3)
             ON CONFLICT (resource) DO UPDATE
               SET owner = EXCLUDED.owner, expires_at = EXCLUDED.expires_at
               WHERE leases.owner = EXCLUDED.owner
                  OR leases.expires_at < (extract(epoch from now()) * 1000)::bigint
             RETURNING owner, expires_at",
        )
        .bind(resource)
        .bind(owner)
        .bind(ttl_ms)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;

        match row {
            Some(r) => Ok(Lease {
                owner: r.get::<String, _>("owner"),
                expires_at: Timestamp(r.get::<i64, _>("expires_at")),
            }),
            None => {
                // Not acquired: a valid lease is still held by someone else.
                let r = sqlx::query("SELECT owner, expires_at FROM leases WHERE resource = $1")
                    .bind(resource)
                    .fetch_one(self.pool())
                    .await
                    .map_err(db_err)?;
                Ok(Lease {
                    owner: r.get::<String, _>("owner"),
                    expires_at: Timestamp(r.get::<i64, _>("expires_at")),
                })
            }
        }
    }
}

#[async_trait]
impl ForgeConnectionStore for PostgresDb {
    async fn put_connection(&self, conn: &ForgeConnection) -> Result<(), RegistryError> {
        sqlx::query(
            "INSERT INTO forge_connections (id, kind, base_url, credential_ref)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE SET
                 kind = EXCLUDED.kind,
                 base_url = EXCLUDED.base_url,
                 credential_ref = EXCLUDED.credential_ref",
        )
        .bind(&conn.id)
        .bind(conn.kind.as_str())
        .bind(&conn.base_url)
        .bind(&conn.credential_ref)
        .execute(self.pool())
        .await
        .map_err(reg_err)?;
        Ok(())
    }

    async fn get_connection(&self, id: &str) -> Result<Option<ForgeConnection>, RegistryError> {
        let row = sqlx::query(
            "SELECT id, kind, base_url, credential_ref FROM forge_connections WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(reg_err)?;
        row.map(connection_from_row).transpose()
    }

    async fn list_connections(&self) -> Result<Vec<ForgeConnection>, RegistryError> {
        let rows = sqlx::query(
            "SELECT id, kind, base_url, credential_ref FROM forge_connections ORDER BY id",
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

    async fn bind_repo(
        &self,
        connection_id: &str,
        repo: &RepoRef,
        org: &str,
        project: &str,
    ) -> Result<(), RegistryError> {
        sqlx::query(
            "INSERT INTO forge_repos (connection_id, owner, name, org, project)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (owner, name) DO UPDATE SET
                 connection_id = EXCLUDED.connection_id,
                 org = EXCLUDED.org,
                 project = EXCLUDED.project",
        )
        .bind(connection_id)
        .bind(&repo.owner)
        .bind(&repo.name)
        .bind(org)
        .bind(project)
        .execute(self.pool())
        .await
        .map_err(reg_err)?;
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

    async fn resolve(&self, repo: &RepoRef) -> Result<Option<ResolvedRepo>, RegistryError> {
        let row = sqlx::query(
            "SELECT c.id, c.kind, c.base_url, c.credential_ref, r.org, r.project
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
    Ok(ForgeConnection {
        id: r.get::<String, _>("id"),
        kind: ForgeKind::from_str_token(&kind)
            .ok_or_else(|| RegistryError::Store(format!("unknown forge kind {kind:?}")))?,
        base_url: r.get::<String, _>("base_url"),
        credential_ref: r.get::<String, _>("credential_ref"),
    })
}

fn reg_err(e: sqlx::Error) -> RegistryError {
    RegistryError::Store(e.to_string())
}

#[async_trait::async_trait]
impl EnvironmentStore for PostgresDb {
    async fn put_environment(
        &self,
        org: &str,
        project: &str,
        env: &Environment,
    ) -> Result<(), ProjectError> {
        let protection = serde_json::to_value(&env.protection)
            .map_err(|e| ProjectError::Store(e.to_string()))?;
        sqlx::query(
            "INSERT INTO environments (org, project, name, protection) VALUES ($1, $2, $3, $4)
             ON CONFLICT (org, project, name) DO UPDATE SET protection = EXCLUDED.protection",
        )
        .bind(org)
        .bind(project)
        .bind(&env.name)
        .bind(protection)
        .execute(self.pool())
        .await
        .map_err(|e| ProjectError::Store(e.to_string()))?;
        Ok(())
    }

    async fn get_environment(
        &self,
        org: &str,
        project: &str,
        name: &str,
    ) -> Result<Option<Environment>, ProjectError> {
        let row = sqlx::query(
            "SELECT protection FROM environments WHERE org = $1 AND project = $2 AND name = $3",
        )
        .bind(org)
        .bind(project)
        .bind(name)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| ProjectError::Store(e.to_string()))?;
        row.map(|r| {
            let protection = serde_json::from_value(r.get::<Value, _>("protection"))
                .map_err(|e| ProjectError::Store(e.to_string()))?;
            Ok(Environment {
                name: name.to_string(),
                protection,
            })
        })
        .transpose()
    }

    async fn list_environments(
        &self,
        org: &str,
        project: &str,
    ) -> Result<Vec<Environment>, ProjectError> {
        let rows = sqlx::query(
            "SELECT name, protection FROM environments WHERE org = $1 AND project = $2 ORDER BY name",
        )
        .bind(org)
        .bind(project)
        .fetch_all(self.pool())
        .await
        .map_err(|e| ProjectError::Store(e.to_string()))?;
        rows.into_iter()
            .map(|r| {
                let protection = serde_json::from_value(r.get::<Value, _>("protection"))
                    .map_err(|e| ProjectError::Store(e.to_string()))?;
                Ok(Environment {
                    name: r.get::<String, _>("name"),
                    protection,
                })
            })
            .collect()
    }

    async fn delete_environment(
        &self,
        org: &str,
        project: &str,
        name: &str,
    ) -> Result<(), ProjectError> {
        sqlx::query("DELETE FROM environments WHERE org = $1 AND project = $2 AND name = $3")
            .bind(org)
            .bind(project)
            .bind(name)
            .execute(self.pool())
            .await
            .map_err(|e| ProjectError::Store(e.to_string()))?;
        Ok(())
    }

    async fn record_deployment(&self, d: &Deployment) -> Result<(), ProjectError> {
        let approved =
            serde_json::to_value(&d.approved_by).map_err(|e| ProjectError::Store(e.to_string()))?;
        sqlx::query(
            "INSERT INTO deployments (org, project, environment, git_ref, run_id, approved_by, at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&d.org)
        .bind(&d.project)
        .bind(&d.environment)
        .bind(&d.git_ref)
        .bind(&d.run)
        .bind(approved)
        .bind(d.at)
        .execute(self.pool())
        .await
        .map_err(|e| ProjectError::Store(e.to_string()))?;
        Ok(())
    }

    async fn deployments(
        &self,
        org: &str,
        project: &str,
        environment: &str,
    ) -> Result<Vec<Deployment>, ProjectError> {
        let rows = sqlx::query(
            "SELECT git_ref, run_id, approved_by, at FROM deployments
             WHERE org = $1 AND project = $2 AND environment = $3 ORDER BY id DESC",
        )
        .bind(org)
        .bind(project)
        .bind(environment)
        .fetch_all(self.pool())
        .await
        .map_err(|e| ProjectError::Store(e.to_string()))?;
        rows.into_iter()
            .map(|r| {
                let approved_by = serde_json::from_value(r.get::<Value, _>("approved_by"))
                    .map_err(|e| ProjectError::Store(e.to_string()))?;
                Ok(Deployment {
                    org: org.to_string(),
                    project: project.to_string(),
                    environment: environment.to_string(),
                    git_ref: r.get::<String, _>("git_ref"),
                    run: r.get::<String, _>("run_id"),
                    approved_by,
                    at: r.get::<i64, _>("at"),
                })
            })
            .collect()
    }
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
        other => Err(DbError::Other(format!("unknown failure kind {other:?}"))),
    }
}

fn db_err(e: sqlx::Error) -> DbError {
    match e {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => DbError::Unavailable,
        other => DbError::Other(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// SessionStore (ADR-0049 C1): server-side login sessions in Postgres.
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
            "INSERT INTO sessions (id, principal, csrf, expires_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE SET
                 principal = EXCLUDED.principal,
                 csrf = EXCLUDED.csrf,
                 expires_at = EXCLUDED.expires_at",
        )
        .bind(&session.id)
        .bind(&principal)
        .bind(&session.csrf)
        .bind(session.expires_at)
        .execute(self.pool())
        .await
        .map_err(|e| scarab_identity::IdentityError::Issuance(e.to_string()))?;
        Ok(())
    }

    async fn get(
        &self,
        id: &str,
    ) -> Result<Option<scarab_identity::Session>, scarab_identity::IdentityError> {
        let row = sqlx::query("SELECT principal, csrf, expires_at FROM sessions WHERE id = $1")
            .bind(id)
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
            csrf: row.get::<String, _>("csrf"),
            expires_at: row.get::<i64, _>("expires_at"),
        }))
    }

    async fn delete(&self, id: &str) -> Result<(), scarab_identity::IdentityError> {
        sqlx::query("DELETE FROM sessions WHERE id = $1")
            .bind(id)
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

fn role_str(role: scarab_identity::Role) -> &'static str {
    match role {
        scarab_identity::Role::Viewer => "viewer",
        scarab_identity::Role::Member => "member",
        scarab_identity::Role::Admin => "admin",
        scarab_identity::Role::Owner => "owner",
    }
}

fn role_from_str(s: &str) -> Option<scarab_identity::Role> {
    Some(match s {
        "viewer" => scarab_identity::Role::Viewer,
        "member" => scarab_identity::Role::Member,
        "admin" => scarab_identity::Role::Admin,
        "owner" => scarab_identity::Role::Owner,
        _ => return None,
    })
}

fn identity_err(e: sqlx::Error) -> scarab_identity::IdentityError {
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

    async fn bindings(
        &self,
        org: &str,
    ) -> Result<Vec<scarab_identity::Binding>, scarab_identity::IdentityError> {
        let rows = sqlx::query(
            "SELECT subject, project, role FROM rbac_bindings
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
                Some(scarab_identity::Binding {
                    subject: r.get::<String, _>("subject"),
                    scope,
                    role,
                })
            })
            .collect())
    }
}
