//! The cron schedule index and fired slots (git-bug 5d5fc8a), migration 0078.

use async_trait::async_trait;
use sqlx::Row;

use scarab_engine::{DbError, ScheduleEntry, ScheduleFire, ScheduleStore, Timestamp};

use crate::{db_err, PostgresDb};

fn entry(r: &sqlx::postgres::PgRow) -> ScheduleEntry {
    ScheduleEntry {
        org: r.get("org"),
        project: r.get("project"),
        pipeline: r.get("pipeline"),
        schedule: r.get("schedule"),
        next_fire_at: Timestamp(r.get::<i64, _>("next_fire_at")),
        source_sha: r.get("source_sha"),
    }
}

#[async_trait]
impl ScheduleStore for PostgresDb {
    async fn replace_project_schedules(
        &self,
        org: &str,
        project: &str,
        source_sha: &str,
        entries: &[ScheduleEntry],
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await.map_err(db_err)?;
        let pipelines: Vec<&str> = entries.iter().map(|e| e.pipeline.as_str()).collect();
        let schedules: Vec<&str> = entries.iter().map(|e| e.schedule.as_str()).collect();
        let next: Vec<i64> = entries.iter().map(|e| e.next_fire_at.0).collect();
        // Rows no longer declared go; the rest keep their next slot.
        sqlx::query(
            "DELETE FROM pipeline_schedules s
             WHERE s.org = $1 AND s.project = $2
               AND NOT EXISTS (
                 SELECT 1 FROM UNNEST($3::text[], $4::text[]) AS k(pipeline, schedule)
                 WHERE k.pipeline = s.pipeline AND k.schedule = s.schedule)",
        )
        .bind(org)
        .bind(project)
        .bind(&pipelines)
        .bind(&schedules)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            "INSERT INTO pipeline_schedules
               (org, project, pipeline, schedule, next_fire_at, source_sha)
             SELECT $1, $2, k.pipeline, k.schedule, k.next_fire_at, $6
             FROM UNNEST($3::text[], $4::text[], $5::bigint[])
               AS k(pipeline, schedule, next_fire_at)
             ON CONFLICT (org, project, pipeline, schedule)
             DO UPDATE SET source_sha = EXCLUDED.source_sha",
        )
        .bind(org)
        .bind(project)
        .bind(&pipelines)
        .bind(&schedules)
        .bind(&next)
        .bind(source_sha)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)
    }

    async fn project_schedules(
        &self,
        org: &str,
        project: &str,
    ) -> Result<Vec<ScheduleEntry>, DbError> {
        let rows = sqlx::query(
            "SELECT org, project, pipeline, schedule, next_fire_at, source_sha
             FROM pipeline_schedules WHERE org = $1 AND project = $2
             ORDER BY pipeline, schedule",
        )
        .bind(org)
        .bind(project)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows.iter().map(entry).collect())
    }

    async fn due_schedules(
        &self,
        before: Timestamp,
        limit: u32,
    ) -> Result<Vec<ScheduleEntry>, DbError> {
        let rows = sqlx::query(
            "SELECT org, project, pipeline, schedule, next_fire_at, source_sha
             FROM pipeline_schedules WHERE next_fire_at <= $1
             ORDER BY next_fire_at, org, project, pipeline, schedule
             LIMIT $2",
        )
        .bind(before.0)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows.iter().map(entry).collect())
    }

    async fn advance_schedule(
        &self,
        e: &ScheduleEntry,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "UPDATE pipeline_schedules SET next_fire_at = $6
             WHERE org = $1 AND project = $2 AND pipeline = $3 AND schedule = $4
               AND next_fire_at = $5",
        )
        .bind(&e.org)
        .bind(&e.project)
        .bind(&e.pipeline)
        .bind(&e.schedule)
        .bind(from.0)
        .bind(to.0)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(done.rows_affected() == 1)
    }

    async fn claim_fire(&self, f: &ScheduleFire) -> Result<bool, DbError> {
        let done = sqlx::query(
            "INSERT INTO schedule_fires
               (org, project, pipeline, schedule, slot_at, fired_at, outcome, run_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (org, project, pipeline, slot_at) DO NOTHING",
        )
        .bind(&f.org)
        .bind(&f.project)
        .bind(&f.pipeline)
        .bind(&f.schedule)
        .bind(f.slot_at.0)
        .bind(f.fired_at.0)
        .bind(&f.outcome)
        .bind(f.run_id.as_deref())
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(done.rows_affected() == 1)
    }

    async fn finish_fire(
        &self,
        org: &str,
        project: &str,
        pipeline: &str,
        slot_at: Timestamp,
        outcome: &str,
        run_id: Option<&str>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE schedule_fires SET outcome = $5, run_id = $6
             WHERE org = $1 AND project = $2 AND pipeline = $3 AND slot_at = $4",
        )
        .bind(org)
        .bind(project)
        .bind(pipeline)
        .bind(slot_at.0)
        .bind(outcome)
        .bind(run_id)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn last_fires(&self, org: &str, project: &str) -> Result<Vec<ScheduleFire>, DbError> {
        let rows = sqlx::query(
            "SELECT DISTINCT ON (pipeline)
                    org, project, pipeline, schedule, slot_at, fired_at, outcome, run_id
             FROM schedule_fires WHERE org = $1 AND project = $2
             ORDER BY pipeline, slot_at DESC",
        )
        .bind(org)
        .bind(project)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .iter()
            .map(|r| ScheduleFire {
                org: r.get("org"),
                project: r.get("project"),
                pipeline: r.get("pipeline"),
                schedule: r.get("schedule"),
                slot_at: Timestamp(r.get::<i64, _>("slot_at")),
                fired_at: Timestamp(r.get::<i64, _>("fired_at")),
                outcome: r.get("outcome"),
                run_id: r.get("run_id"),
            })
            .collect())
    }

    async fn prune_fires(&self, before: Timestamp) -> Result<u64, DbError> {
        let done = sqlx::query("DELETE FROM schedule_fires WHERE fired_at < $1")
            .bind(before.0)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        Ok(done.rows_affected())
    }
}
