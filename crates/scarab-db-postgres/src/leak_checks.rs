//! [`LeakCheckLimitsStore`] on Postgres (migration 0109): each org's limits
//! on leak checks.

use async_trait::async_trait;
use sqlx::Row;

use scarab_identity::{IdentityError, LeakCheckLimits, LeakCheckLimitsStore};

use crate::{identity_err, PostgresDb};

/// A stored limit as a `u32`. The table's CHECKs keep it in 0..=1000, so a
/// negative one is a row this binary cannot read: refused, never clamped.
fn limit(row: &sqlx::postgres::PgRow, column: &str) -> Result<u32, IdentityError> {
    u32::try_from(row.get::<i32, _>(column)).map_err(|_| {
        IdentityError::Issuance(format!(
            "`org_leak_check_limits.{column}` holds an unreadable limit"
        ))
    })
}

#[async_trait]
impl LeakCheckLimitsStore for PostgresDb {
    async fn leak_check_limits(&self, org: &str) -> Result<Option<LeakCheckLimits>, IdentityError> {
        let row = sqlx::query("SELECT per_hour, per_day FROM org_leak_check_limits WHERE org = $1")
            .bind(org)
            .fetch_optional(self.pool())
            .await
            .map_err(identity_err)?;
        row.map(|r| {
            Ok(LeakCheckLimits {
                per_hour: limit(&r, "per_hour")?,
                per_day: limit(&r, "per_day")?,
            })
        })
        .transpose()
    }

    async fn put_leak_check_limits(
        &self,
        org: &str,
        limits: LeakCheckLimits,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<(), IdentityError> {
        let per_hour = i32::try_from(limits.per_hour)
            .map_err(|_| IdentityError::Conflict("the hourly limit is out of range".into()))?;
        let per_day = i32::try_from(limits.per_day)
            .map_err(|_| IdentityError::Conflict("the daily limit is out of range".into()))?;
        sqlx::query(
            "INSERT INTO org_leak_check_limits (org, per_hour, per_day, updated_by, updated_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (org) DO UPDATE
               SET per_hour = EXCLUDED.per_hour,
                   per_day = EXCLUDED.per_day,
                   updated_by = EXCLUDED.updated_by,
                   updated_at = EXCLUDED.updated_at",
        )
        .bind(org)
        .bind(per_hour)
        .bind(per_day)
        .bind(updated_by)
        .bind(updated_at)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(())
    }
}
