//! Webhook delivery outcomes and the delivery row's try (ADR-0083): the SQL
//! the `Db`, `ForgeConnectionStore` and `PolicyStore` implementations share.

use sqlx::Row;

use scarab_engine::{
    DbError, DeliveryOutcome, DeliveryOutcomeKind, DeliveryPipeline, DeliveryRefusal, HeadQuery,
    OutcomeWrite, RunCreationState, RunId, Timestamp,
};
use scarab_forge::{DeliveryPins, DeliveryTry, ForgeKind, RegistryError};

use crate::{db_err, insert_outbox, reg_err, run_status_from_str, PostgresDb};

/// The database clock in unix ms: the delivery row's `at` and its lease are
/// read and written with it, so every replica agrees on them.
const NOW_MS: &str = "(extract(epoch from now()) * 1000)::bigint";

/// A fresh lease token for a try (ADR-0083 §7), made by the database.
const NEW_TOKEN: &str = "md5(random()::text || clock_timestamp()::text)";

/// The outcome key and the run a first row claims it for.
pub(crate) struct RunClaim<'a> {
    pub connection_id: &'a str,
    pub delivery_id: &'a str,
    pub pipeline: &'a str,
    pub forge: &'a str,
    pub replaces: Option<&'a str>,
    pub at: i64,
}

impl<'a> RunClaim<'a> {
    pub(crate) fn of(claim: &'a scarab_engine::DeliveryRunClaim) -> Self {
        Self {
            connection_id: &claim.key.connection_id,
            delivery_id: &claim.key.delivery_id,
            pipeline: &claim.key.pipeline,
            forge: &claim.key.forge,
            replaces: claim.replaces.as_ref().map(|r| r.0.as_str()),
            at: claim.at.0,
        }
    }

    pub(crate) fn of_policy_row(d: &'a scarab_policy::store::RunCreationDelivery, at: i64) -> Self {
        Self {
            connection_id: &d.connection_id,
            delivery_id: &d.delivery_id,
            pipeline: &d.pipeline,
            forge: &d.forge,
            replaces: d.replaces.as_deref(),
            at,
        }
    }
}

/// Lock `run` for its replacement (ADR-0083 §4) and say whether it may be
/// replaced: its creation never finished, it has ended (so no stalled try
/// can still finish it: `finish_run_creation` needs `pending`), and nothing
/// replaced it yet. The row lock holds until the transaction ends, so a
/// concurrent finish or replacement waits and then finds it changed.
async fn lock_replaceable(tx: &mut sqlx::PgConnection, run: &str) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "SELECT 1 FROM runs
         WHERE id = $1 AND creating AND status <> 'pending' AND replaced_by IS NULL
         FOR UPDATE",
    )
    .bind(run)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(row.is_some())
}

/// Inside the transaction of `run`'s first row: record the outcome `run`
/// for `claim`'s key, or move it from `claim.replaces` to `run` and mark that
/// run replaced. `false` when the key is already recorded (or no longer
/// names `claim.replaces`): the caller rolls back. A concurrent insert of the
/// same key makes this wait for it, then answer `false`; no unique violation
/// is raised.
pub(crate) async fn claim_delivery_run(
    tx: &mut sqlx::PgConnection,
    claim: &RunClaim<'_>,
    run: &str,
) -> Result<bool, sqlx::Error> {
    let Some(old) = claim.replaces else {
        let inserted = sqlx::query(
            "INSERT INTO webhook_delivery_pipelines
               (connection_id, delivery_id, pipeline, forge, outcome, run_id, at)
             VALUES ($1, $2, $3, $4, 'run', $5, $6)
             ON CONFLICT (connection_id, delivery_id, pipeline) DO NOTHING
             RETURNING 1",
        )
        .bind(claim.connection_id)
        .bind(claim.delivery_id)
        .bind(claim.pipeline)
        .bind(claim.forge)
        .bind(run)
        .bind(claim.at)
        .fetch_optional(&mut *tx)
        .await?;
        return Ok(inserted.is_some());
    };
    if !lock_replaceable(tx, old).await? {
        return Ok(false);
    }
    let moved = sqlx::query(
        "UPDATE webhook_delivery_pipelines SET run_id = $4
         WHERE connection_id = $1 AND delivery_id = $2 AND pipeline = $3
           AND outcome = 'run' AND run_id = $5
         RETURNING 1",
    )
    .bind(claim.connection_id)
    .bind(claim.delivery_id)
    .bind(claim.pipeline)
    .bind(run)
    .bind(old)
    .fetch_optional(&mut *tx)
    .await?;
    if moved.is_none() {
        return Ok(false);
    }
    sqlx::query("UPDATE runs SET replaced_by = $2 WHERE id = $1")
        .bind(old)
        .bind(run)
        .execute(&mut *tx)
        .await?;
    Ok(true)
}

fn outcome_from_row(r: &sqlx::postgres::PgRow) -> Result<DeliveryOutcome, DbError> {
    let kind: String = r.get("outcome");
    Ok(DeliveryOutcome {
        pipeline: r.get("pipeline"),
        outcome: DeliveryOutcomeKind::parse(&kind)
            .ok_or_else(|| DbError::Other(format!("unknown delivery outcome `{kind}`")))?,
        reason_kind: r.get("reason_kind"),
        run_id: r.get::<Option<String>, _>("run_id").map(RunId),
        refused_run_id: r.get("refused_run_id"),
        at: Timestamp(r.get("at")),
    })
}

const OUTCOME_COLUMNS: &str = "pipeline, outcome, reason_kind, run_id, refused_run_id, at";

/// The outcome recorded for one key, read after a write lost to it.
pub(crate) async fn read_outcome(
    db: &PostgresDb,
    connection_id: &str,
    delivery_id: &str,
    pipeline: &str,
) -> Result<Option<DeliveryOutcome>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {OUTCOME_COLUMNS} FROM webhook_delivery_pipelines
         WHERE connection_id = $1 AND delivery_id = $2 AND pipeline = $3"
    ))
    .bind(connection_id)
    .bind(delivery_id)
    .bind(pipeline)
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)?;
    row.as_ref().map(outcome_from_row).transpose()
}

async fn claimed(db: &PostgresDb, key: &DeliveryPipeline) -> Result<OutcomeWrite, DbError> {
    Ok(OutcomeWrite::Claimed(
        read_outcome(db, &key.connection_id, &key.delivery_id, &key.pipeline).await?,
    ))
}

pub(crate) async fn begin_delivery_run_creation(
    db: &PostgresDb,
    run: &RunId,
    ir_version: u32,
    event_schema_version: u32,
    claim: &scarab_engine::DeliveryRunClaim,
) -> Result<OutcomeWrite, DbError> {
    let mut tx = db.pool().begin().await.map_err(db_err)?;
    if !claim_delivery_run(&mut tx, &RunClaim::of(claim), &run.0)
        .await
        .map_err(db_err)?
    {
        tx.rollback().await.map_err(db_err)?;
        return claimed(db, &claim.key).await;
    }
    crate::insert_creating_run(
        &mut *tx,
        &run.0,
        ir_version,
        event_schema_version,
        claim.at.0,
    )
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    Ok(OutcomeWrite::Written)
}

pub(crate) async fn run_creation_state(
    db: &PostgresDb,
    run: &RunId,
) -> Result<Option<RunCreationState>, DbError> {
    let row =
        sqlx::query("SELECT status, creating, created_at, replaced_by FROM runs WHERE id = $1")
            .bind(&run.0)
            .fetch_optional(db.pool())
            .await
            .map_err(db_err)?;
    row.map(|r| {
        Ok(RunCreationState {
            status: run_status_from_str(r.get("status"))?,
            creating: r.get("creating"),
            created_at: Timestamp(r.get("created_at")),
            replaced_by: r.get::<Option<String>, _>("replaced_by").map(RunId),
        })
    })
    .transpose()
}

pub(crate) async fn insert_refused_run<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    r: &scarab_engine::RefusedRun,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO refused_runs (org, project, pipeline, trigger_kind, actor, git_ref, sha,
                                   title, reason_kind, message, errors, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         RETURNING id",
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
    .fetch_one(exec)
    .await
}

pub(crate) async fn record_delivery_refusal(
    db: &PostgresDb,
    key: &DeliveryPipeline,
    refusal: &DeliveryRefusal,
) -> Result<OutcomeWrite, DbError> {
    let mut tx = db.pool().begin().await.map_err(db_err)?;
    let refused_id = match &refusal.refused {
        Some(r) => Some(insert_refused_run(&mut *tx, r).await.map_err(db_err)?),
        None => None,
    };
    let written = match &refusal.replaces {
        None => sqlx::query(
            "INSERT INTO webhook_delivery_pipelines
               (connection_id, delivery_id, pipeline, forge, outcome, reason_kind,
                refused_run_id, at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (connection_id, delivery_id, pipeline) DO NOTHING
             RETURNING 1",
        )
        .bind(&key.connection_id)
        .bind(&key.delivery_id)
        .bind(&key.pipeline)
        .bind(&key.forge)
        .bind(refusal.outcome.as_str())
        .bind(&refusal.reason_kind)
        .bind(refused_id)
        .bind(refusal.at.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?,
        Some(old) if !lock_replaceable(&mut tx, &old.0).await.map_err(db_err)? => None,
        Some(old) => sqlx::query(
            "UPDATE webhook_delivery_pipelines
             SET outcome = $4, reason_kind = $5, run_id = NULL, refused_run_id = $6
             WHERE connection_id = $1 AND delivery_id = $2 AND pipeline = $3
               AND outcome = 'run' AND run_id = $7
             RETURNING 1",
        )
        .bind(&key.connection_id)
        .bind(&key.delivery_id)
        .bind(&key.pipeline)
        .bind(refusal.outcome.as_str())
        .bind(&refusal.reason_kind)
        .bind(refused_id)
        .bind(&old.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?,
    };
    if written.is_none() {
        tx.rollback().await.map_err(db_err)?;
        return claimed(db, key).await;
    }
    if let Some(status) = &refusal.status {
        insert_outbox(&mut *tx, status).await?;
    }
    tx.commit().await.map_err(db_err)?;
    Ok(OutcomeWrite::Written)
}

pub(crate) async fn record_delivery_outcome(
    db: &PostgresDb,
    key: &DeliveryPipeline,
    outcome: DeliveryOutcomeKind,
    at: Timestamp,
) -> Result<OutcomeWrite, DbError> {
    let written = sqlx::query(
        "INSERT INTO webhook_delivery_pipelines
           (connection_id, delivery_id, pipeline, forge, outcome, at)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (connection_id, delivery_id, pipeline) DO NOTHING
         RETURNING 1",
    )
    .bind(&key.connection_id)
    .bind(&key.delivery_id)
    .bind(&key.pipeline)
    .bind(&key.forge)
    .bind(outcome.as_str())
    .bind(at.0)
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)?;
    if written.is_none() {
        return claimed(db, key).await;
    }
    Ok(OutcomeWrite::Written)
}

pub(crate) async fn delivery_outcomes(
    db: &PostgresDb,
    connection_id: &str,
    delivery_id: &str,
) -> Result<Vec<DeliveryOutcome>, DbError> {
    let rows = sqlx::query(&format!(
        "SELECT {OUTCOME_COLUMNS} FROM webhook_delivery_pipelines
         WHERE connection_id = $1 AND delivery_id = $2
         ORDER BY pipeline COLLATE \"C\""
    ))
    .bind(connection_id)
    .bind(delivery_id)
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    rows.iter().map(outcome_from_row).collect()
}

pub(crate) async fn head_moved(db: &PostgresDb, q: &HeadQuery) -> Result<bool, DbError> {
    // Deliveries are ordered by arrival on the database clock (ADR-0083 §5).
    sqlx::query_scalar(
        "SELECT EXISTS (
           SELECT 1 FROM webhook_deliveries
           WHERE head_scope = $1 AND at > $2 AND head_sha <> $3)",
    )
    .bind(q.scope.key(&q.org, &q.project))
    .bind(q.after.0)
    .bind(&q.head_sha)
    .fetch_one(db.pool())
    .await
    .map_err(db_err)
}

pub(crate) async fn prune_webhook_deliveries(
    db: &PostgresDb,
    before: Timestamp,
    limit: u32,
) -> Result<u64, DbError> {
    // One statement, so one transaction: a delivery and its outcomes go
    // together, and no later try can find some of its outcomes and not
    // others (ADR-0083 §8).
    // A delivery a try holds stays: its try may still record outcomes.
    let done = sqlx::query(&format!(
        "WITH gone AS (
           SELECT forge, id FROM webhook_deliveries
           WHERE at < $1 AND (try_until IS NULL OR try_until < {NOW_MS})
           ORDER BY at LIMIT $2
           FOR UPDATE SKIP LOCKED
         ), outcomes AS (
           DELETE FROM webhook_delivery_pipelines p USING gone
           WHERE p.forge = gone.forge AND p.delivery_id = gone.id
         )
         DELETE FROM webhook_deliveries w USING gone
         WHERE w.forge = gone.forge AND w.id = gone.id"
    ))
    .bind(before.0)
    .bind(i64::from(limit))
    .execute(db.pool())
    .await
    .map_err(db_err)?;
    Ok(done.rows_affected())
}

pub(crate) async fn begin_delivery_try(
    db: &PostgresDb,
    forge: ForgeKind,
    delivery_id: &str,
    lease_ms: i64,
) -> Result<DeliveryTry, RegistryError> {
    let inserted = sqlx::query(&format!(
        "INSERT INTO webhook_deliveries (forge, id, at, unfinished, try_until, try_token)
         VALUES ($1, $2, {NOW_MS}, TRUE, {NOW_MS} + $3, {NEW_TOKEN})
         ON CONFLICT (forge, id) DO NOTHING
         RETURNING at, try_token"
    ))
    .bind(forge.as_str())
    .bind(delivery_id)
    .bind(lease_ms)
    .fetch_optional(db.pool())
    .await
    .map_err(reg_err)?;
    if let Some(r) = inserted {
        return Ok(DeliveryTry::First {
            at: r.get("at"),
            token: r.get("try_token"),
        });
    }
    let taken = sqlx::query(&format!(
        "UPDATE webhook_deliveries SET try_until = {NOW_MS} + $3, try_token = {NEW_TOKEN}
         WHERE forge = $1 AND id = $2 AND unfinished
           AND (try_until IS NULL OR try_until < {NOW_MS})
         RETURNING at, config_sha, head_scope, head_sha, try_token"
    ))
    .bind(forge.as_str())
    .bind(delivery_id)
    .bind(lease_ms)
    .fetch_optional(db.pool())
    .await
    .map_err(reg_err)?;
    if let Some(r) = taken {
        let pins = r
            .get::<Option<String>, _>("config_sha")
            .map(|config_sha| DeliveryPins {
                config_sha,
                head_scope: r.get("head_scope"),
                head_sha: r.get("head_sha"),
            });
        return Ok(DeliveryTry::Again {
            at: r.get("at"),
            pins,
            token: r.get("try_token"),
        });
    }
    let unfinished: Option<bool> = sqlx::query_scalar(
        "SELECT unfinished FROM webhook_deliveries WHERE forge = $1 AND id = $2",
    )
    .bind(forge.as_str())
    .bind(delivery_id)
    .fetch_optional(db.pool())
    .await
    .map_err(reg_err)?;
    // A row pruned between the two statements is a delivery older than the
    // retention period: answered as held, and its next redelivery is a first
    // try.
    Ok(match unfinished {
        Some(false) => DeliveryTry::Finished,
        Some(true) | None => DeliveryTry::Held,
    })
}

pub(crate) async fn pin_delivery(
    db: &PostgresDb,
    forge: ForgeKind,
    delivery_id: &str,
    pins: &DeliveryPins,
) -> Result<DeliveryPins, RegistryError> {
    // The first pin wins: every SET expression reads the row as it was.
    let row = sqlx::query(
        "UPDATE webhook_deliveries SET
           config_sha = COALESCE(config_sha, $3),
           head_scope = CASE WHEN config_sha IS NULL THEN $4 ELSE head_scope END,
           head_sha = CASE WHEN config_sha IS NULL THEN $5 ELSE head_sha END
         WHERE forge = $1 AND id = $2
         RETURNING config_sha, head_scope, head_sha",
    )
    .bind(forge.as_str())
    .bind(delivery_id)
    .bind(&pins.config_sha)
    .bind(pins.head_scope.as_deref())
    .bind(pins.head_sha.as_deref())
    .fetch_optional(db.pool())
    .await
    .map_err(reg_err)?;
    Ok(match row {
        Some(r) => DeliveryPins {
            config_sha: r.get("config_sha"),
            head_scope: r.get("head_scope"),
            head_sha: r.get("head_sha"),
        },
        None => pins.clone(),
    })
}

pub(crate) async fn end_delivery_try(
    db: &PostgresDb,
    forge: ForgeKind,
    delivery_id: &str,
    token: &str,
    finished: bool,
) -> Result<(), RegistryError> {
    // Only the lease holder ends the try (ADR-0083 §7): a try whose lease
    // lapsed and was taken leaves the later try's lease alone.
    sqlx::query(
        "UPDATE webhook_deliveries
         SET try_until = NULL, try_token = NULL, unfinished = unfinished AND NOT $3
         WHERE forge = $1 AND id = $2 AND try_token = $4",
    )
    .bind(forge.as_str())
    .bind(delivery_id)
    .bind(finished)
    .bind(token)
    .execute(db.pool())
    .await
    .map_err(reg_err)?;
    Ok(())
}
