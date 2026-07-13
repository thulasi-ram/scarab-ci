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
    Attempt, AttemptId, ConcurrencyPolicy, Db, DbError, EventKind, EventPayload, FailureKind,
    LogChunkMeta, OutboxId, OutboxMessage, RunId, RunStatus, RunSummary, StepId, StepRun, StepSpec,
    StepStatus, Timestamp,
};
use scarab_projects::{Deployment, Environment, EnvironmentStore, ProjectError};

/// The embedded, ordered set of forward-only migrations. `MIGRATOR.run(pool)`
/// applies all pending ones (tracked in `_sqlx_migrations`); tests can also walk
/// `MIGRATOR.iter()` to apply schema versions one at a time.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// A Postgres-backed [`Db`]. Holds an optional connection pool so the
/// composition root can construct it without performing I/O.
pub struct PostgresDb {
    pool: Option<PgPool>,
}

impl PostgresDb {
    /// Construct without connecting (pool wired later).
    pub fn new() -> Self {
        Self { pool: None }
    }

    /// Construct from an existing connection pool.
    pub fn with_pool(pool: PgPool) -> Self {
        Self { pool: Some(pool) }
    }

    /// Connect a pool to `url`.
    pub async fn connect(url: &str) -> Result<Self, DbError> {
        let pool = PgPool::connect(url).await.map_err(db_err)?;
        Ok(Self::with_pool(pool))
    }

    /// Apply all pending migrations (the production expand-contract path).
    pub async fn migrate(&self) -> Result<(), DbError> {
        MIGRATOR.run(self.pool()?).await.map_err(|e| DbError::Other(e.to_string()))
    }

    fn pool(&self) -> Result<&PgPool, DbError> {
        self.pool.as_ref().ok_or(DbError::Unavailable)
    }

    // --- read helpers ------------------------------------------------------

    /// Current status of a step, if it exists.
    pub async fn step_status(&self, run: &RunId, step: &StepId) -> Result<Option<StepStatus>, DbError> {
        let row = sqlx::query("SELECT status FROM step_runs WHERE run_id = $1 AND step_id = $2")
            .bind(&run.0)
            .bind(&step.0)
            .fetch_optional(self.pool()?)
            .await
            .map_err(db_err)?;
        row.map(|r| step_status_from_str(r.get::<String, _>("status")))
            .transpose()
    }

    /// All attempts of a step, in start order.
    pub async fn attempts(&self, run: &RunId, step: &StepId) -> Result<Vec<Attempt>, DbError> {
        let rows = sqlx::query(
            "SELECT attempt_id, started_at, failure FROM attempts
             WHERE run_id = $1 AND step_id = $2 ORDER BY started_at",
        )
        .bind(&run.0)
        .bind(&step.0)
        .fetch_all(self.pool()?)
        .await
        .map_err(db_err)?;
        rows.into_iter()
            .map(|r| {
                let failure = r
                    .get::<Option<String>, _>("failure")
                    .map(|s| failure_from_str(&s))
                    .transpose()?;
                Ok(Attempt {
                    id: AttemptId(r.get::<String, _>("attempt_id")),
                    started_at: Timestamp(r.get::<i64, _>("started_at")),
                    failure,
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
        .fetch_all(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>("kind")).collect())
    }
}

impl Default for PostgresDb {
    fn default() -> Self {
        Self::new()
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
        .fetch_all(self.pool()?)
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
        .execute(self.pool()?)
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
        .execute(self.pool()?)
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
        .execute(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_ir(&self, run: &RunId) -> Result<Option<serde_json::Value>, DbError> {
        let row = sqlx::query("SELECT ir FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool()?)
            .await
            .map_err(db_err)?;
        // `ir` is itself nullable, so unwrap both the missing-row and NULL cases.
        Ok(row.and_then(|r| r.get::<Option<Value>, _>("ir")))
    }

    async fn set_run_deploy_context(
        &self,
        run: &RunId,
        ctx: &scarab_engine::DeployContext,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE runs SET deploy_org = $2, deploy_repo = $3, deploy_environment = $4,
                 deploy_git_ref = $5, deploy_locked_out = $6,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE id = $1",
        )
        .bind(&run.0)
        .bind(&ctx.org)
        .bind(&ctx.repo)
        .bind(&ctx.environment)
        .bind(&ctx.git_ref)
        .bind(ctx.locked_out)
        .execute(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_deploy_context(
        &self,
        run: &RunId,
    ) -> Result<Option<scarab_engine::DeployContext>, DbError> {
        let row = sqlx::query(
            "SELECT deploy_org, deploy_repo, deploy_environment, deploy_git_ref, deploy_locked_out
             FROM runs WHERE id = $1",
        )
        .bind(&run.0)
        .fetch_optional(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(row.and_then(|r| {
            // The four scope columns are set together (or all NULL for a
            // non-deploy run); `deploy_locked_out` defaults false.
            match (
                r.get::<Option<String>, _>("deploy_org"),
                r.get::<Option<String>, _>("deploy_repo"),
                r.get::<Option<String>, _>("deploy_environment"),
                r.get::<Option<String>, _>("deploy_git_ref"),
            ) {
                (Some(org), Some(repo), Some(environment), Some(git_ref)) => {
                    Some(scarab_engine::DeployContext {
                        org,
                        repo,
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
        snapshot: &str,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE step_runs
             SET output_snapshot = $3,
                 updated_at = (extract(epoch from now()) * 1000)::bigint
             WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(snapshot)
        .execute(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn step_output(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        let row =
            sqlx::query("SELECT output_snapshot FROM step_runs WHERE run_id = $1 AND step_id = $2")
                .bind(&run.0)
                .bind(&step.0)
                .fetch_optional(self.pool()?)
                .await
                .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("output_snapshot")))
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
        .execute(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn step_input(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        let row =
            sqlx::query("SELECT input_signature FROM step_runs WHERE run_id = $1 AND step_id = $2")
                .bind(&run.0)
                .bind(&step.0)
                .fetch_optional(self.pool()?)
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
        .execute(self.pool()?)
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
                .fetch_optional(self.pool()?)
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
        .execute(self.pool()?)
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
                .fetch_optional(self.pool()?)
                .await
                .map_err(db_err)?;
        Ok(row.and_then(|r| {
            let group: Option<String> = r.get("concurrency_group");
            let policy: Option<String> = r.get("concurrency_policy");
            group.map(|g| (g, ConcurrencyPolicy::from_wire(policy.as_deref().unwrap_or("queue"))))
        }))
    }

    async fn acquire_slot(&self, group: &str, run: &RunId) -> Result<Option<RunId>, DbError> {
        // Serialize acquirers on this group with a row lock, so exactly one wins.
        let mut tx = self.pool()?.begin().await.map_err(db_err)?;
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
            .execute(self.pool()?)
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
        .execute(self.pool()?)
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
        .fetch_all(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(|r| RunId(r.get::<String, _>("id"))).collect())
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
        .execute(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn run_project(&self, run: &RunId) -> Result<Option<String>, DbError> {
        let row = sqlx::query("SELECT project FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool()?)
            .await
            .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("project")))
    }

    async fn count_in_flight_runs(&self, project: Option<&str>) -> Result<u32, DbError> {
        let row = sqlx::query(
            "SELECT count(*) AS n FROM runs
             WHERE status IN ('running', 'suspended')
               AND ($1::text IS NULL OR project = $1)",
        )
        .bind(project)
        .fetch_one(self.pool()?)
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
        .execute(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn gate_timer_seconds(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<i64>, DbError> {
        let row = sqlx::query(
            "SELECT gate_timer_seconds FROM step_runs WHERE run_id = $1 AND step_id = $2",
        )
        .bind(&run.0)
        .bind(&step.0)
        .fetch_optional(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(row.and_then(|r| r.get::<Option<i64>, _>("gate_timer_seconds")))
    }

    async fn run_status(&self, run: &RunId) -> Result<Option<RunStatus>, DbError> {
        let row = sqlx::query("SELECT status FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(self.pool()?)
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
        .fetch_all(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(|r| RunId(r.get::<String, _>("id"))).collect())
    }

    async fn list_runs(&self, limit: u32) -> Result<Vec<RunSummary>, DbError> {
        let rows = sqlx::query(
            "SELECT id, status, created_at FROM runs
             ORDER BY created_at DESC, id DESC
             LIMIT $1",
        )
        .bind(limit as i64)
        .fetch_all(self.pool()?)
        .await
        .map_err(db_err)?;
        rows.into_iter()
            .map(|r| {
                Ok(RunSummary {
                    run: RunId(r.get::<String, _>("id")),
                    status: run_status_from_str(r.get::<String, _>("status"))?,
                    created_at: Timestamp(r.get::<i64, _>("created_at")),
                })
            })
            .collect()
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventKind>, DbError> {
        let rows = sqlx::query("SELECT version, at, payload FROM events WHERE run_id = $1 ORDER BY seq")
            .bind(&run.0)
            .fetch_all(self.pool()?)
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
        .fetch_all(self.pool()?)
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
            .fetch_optional(self.pool()?)
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
        .execute(self.pool()?)
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
        .fetch_all(self.pool()?)
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
        .execute(self.pool()?)
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
        // Idempotent on the monotonic attempt id (the fencing unit) — a re-drive
        // records the same attempt rather than a duplicate.
        sqlx::query(
            "INSERT INTO attempts (run_id, step_id, attempt_id, started_at, failure)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (run_id, step_id, attempt_id)
             DO UPDATE SET failure = EXCLUDED.failure",
        )
        .bind(&run.0)
        .bind(&step.0)
        .bind(&attempt.id.0)
        .bind(attempt.started_at.0)
        .bind(attempt.failure.map(failure_str))
        .execute(self.pool()?)
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
        .execute(self.pool()?)
        .await
        .map_err(db_err)?
        .rows_affected();
        if affected == 0 {
            return Err(DbError::Conflict);
        }
        Ok(())
    }

    async fn append_event(&self, event: &EventKind) -> Result<(), DbError> {
        let payload = serde_json::to_value(&event.kind).map_err(|e| DbError::Other(e.to_string()))?;
        sqlx::query("INSERT INTO events (run_id, version, at, payload) VALUES ($1, $2, $3, $4)")
            .bind(&event.run.0)
            .bind(event.version as i32)
            .bind(event.at.0)
            .bind(payload)
            .execute(self.pool()?)
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
        .execute(self.pool()?)
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
                   AND ($4::text IS NULL OR kind = $4)
                   AND (claimed_until IS NULL
                        OR claimed_until < (extract(epoch from now()) * 1000)::bigint)
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
        .fetch_all(self.pool()?)
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
        .execute(self.pool()?)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn lease(&self, resource: &str, owner: &str, ttl_ms: i64) -> Result<Lease, DbError> {
        // Acquire or renew, taking over only an expired lease. RETURNING yields
        // the winning holder; if the incumbent lease is still valid the DO
        // UPDATE is skipped and we read back the current holder instead.
        let row = sqlx::query(
            "INSERT INTO leases (resource, owner, expires_at)
             VALUES ($1, $2, (extract(epoch from now()) * 1000)::bigint + $3)
             ON CONFLICT (resource) DO UPDATE
               SET owner = EXCLUDED.owner, expires_at = EXCLUDED.expires_at
               WHERE leases.expires_at < (extract(epoch from now()) * 1000)::bigint
             RETURNING owner, expires_at",
        )
        .bind(resource)
        .bind(owner)
        .bind(ttl_ms)
        .fetch_optional(self.pool()?)
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
                    .fetch_one(self.pool()?)
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

#[async_trait::async_trait]
impl EnvironmentStore for PostgresDb {
    async fn put_environment(
        &self,
        org: &str,
        repo: &str,
        env: &Environment,
    ) -> Result<(), ProjectError> {
        let protection = serde_json::to_value(&env.protection)
            .map_err(|e| ProjectError::Store(e.to_string()))?;
        sqlx::query(
            "INSERT INTO environments (org, repo, name, protection) VALUES ($1, $2, $3, $4)
             ON CONFLICT (org, repo, name) DO UPDATE SET protection = EXCLUDED.protection",
        )
        .bind(org)
        .bind(repo)
        .bind(&env.name)
        .bind(protection)
        .execute(self.pool().map_err(proj_err)?)
        .await
        .map_err(|e| ProjectError::Store(e.to_string()))?;
        Ok(())
    }

    async fn get_environment(
        &self,
        org: &str,
        repo: &str,
        name: &str,
    ) -> Result<Option<Environment>, ProjectError> {
        let row = sqlx::query(
            "SELECT protection FROM environments WHERE org = $1 AND repo = $2 AND name = $3",
        )
        .bind(org)
        .bind(repo)
        .bind(name)
        .fetch_optional(self.pool().map_err(proj_err)?)
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
        repo: &str,
    ) -> Result<Vec<Environment>, ProjectError> {
        let rows = sqlx::query(
            "SELECT name, protection FROM environments WHERE org = $1 AND repo = $2 ORDER BY name",
        )
        .bind(org)
        .bind(repo)
        .fetch_all(self.pool().map_err(proj_err)?)
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
        repo: &str,
        name: &str,
    ) -> Result<(), ProjectError> {
        sqlx::query("DELETE FROM environments WHERE org = $1 AND repo = $2 AND name = $3")
            .bind(org)
            .bind(repo)
            .bind(name)
            .execute(self.pool().map_err(proj_err)?)
            .await
            .map_err(|e| ProjectError::Store(e.to_string()))?;
        Ok(())
    }

    async fn record_deployment(&self, d: &Deployment) -> Result<(), ProjectError> {
        let approved = serde_json::to_value(&d.approved_by)
            .map_err(|e| ProjectError::Store(e.to_string()))?;
        sqlx::query(
            "INSERT INTO deployments (org, repo, environment, git_ref, run_id, approved_by, at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&d.org)
        .bind(&d.repo)
        .bind(&d.environment)
        .bind(&d.git_ref)
        .bind(&d.run)
        .bind(approved)
        .bind(d.at)
        .execute(self.pool().map_err(proj_err)?)
        .await
        .map_err(|e| ProjectError::Store(e.to_string()))?;
        Ok(())
    }

    async fn deployments(
        &self,
        org: &str,
        repo: &str,
        environment: &str,
    ) -> Result<Vec<Deployment>, ProjectError> {
        let rows = sqlx::query(
            "SELECT git_ref, run_id, approved_by, at FROM deployments
             WHERE org = $1 AND repo = $2 AND environment = $3 ORDER BY id DESC",
        )
        .bind(org)
        .bind(repo)
        .bind(environment)
        .fetch_all(self.pool().map_err(proj_err)?)
        .await
        .map_err(|e| ProjectError::Store(e.to_string()))?;
        rows.into_iter()
            .map(|r| {
                let approved_by = serde_json::from_value(r.get::<Value, _>("approved_by"))
                    .map_err(|e| ProjectError::Store(e.to_string()))?;
                Ok(Deployment {
                    org: org.to_string(),
                    repo: repo.to_string(),
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

fn proj_err(e: DbError) -> ProjectError {
    ProjectError::Store(e.to_string())
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
    let ids: Vec<String> =
        serde_json::from_value(v).map_err(|e| DbError::Other(e.to_string()))?;
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
        FailureKind::Infra => "infra",
        FailureKind::Step => "step",
    }
}

fn failure_from_str(s: &str) -> Result<FailureKind, DbError> {
    match s {
        "infra" => Ok(FailureKind::Infra),
        "step" => Ok(FailureKind::Step),
        other => Err(DbError::Other(format!("unknown failure kind {other:?}"))),
    }
}

fn db_err(e: sqlx::Error) -> DbError {
    match e {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => DbError::Unavailable,
        other => DbError::Other(other.to_string()),
    }
}
