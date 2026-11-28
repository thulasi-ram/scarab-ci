//! [`ForkRunApprovalStore`] on Postgres (migration 0114): each org's Fork
//! Run approval setting.

use async_trait::async_trait;
use sqlx::Row;

use scarab_identity::{ForkRunApproval, ForkRunApprovalStore, IdentityError};

use crate::{identity_err, PostgresDb};

#[async_trait]
impl ForkRunApprovalStore for PostgresDb {
    async fn fork_run_approval(&self, org: &str) -> Result<Option<ForkRunApproval>, IdentityError> {
        let row = sqlx::query("SELECT enabled FROM org_fork_run_approval WHERE org = $1")
            .bind(org)
            .fetch_optional(self.pool())
            .await
            .map_err(identity_err)?;
        Ok(row.map(|r| ForkRunApproval {
            enabled: r.get::<bool, _>("enabled"),
        }))
    }

    async fn put_fork_run_approval(
        &self,
        org: &str,
        setting: ForkRunApproval,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<Option<ForkRunApproval>, IdentityError> {
        let mut tx = self.pool().begin().await.map_err(identity_err)?;
        // An org with no row gets one, and had the setting off before. A
        // concurrent first insert makes this one wait for it and then do
        // nothing, so the row is read below with that insert's setting.
        let inserted = sqlx::query(
            "INSERT INTO org_fork_run_approval (org, enabled, updated_by, updated_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (org) DO NOTHING",
        )
        .bind(org)
        .bind(setting.enabled)
        .bind(updated_by)
        .bind(updated_at)
        .execute(&mut *tx)
        .await
        .map_err(identity_err)?
        .rows_affected();
        if inserted == 1 {
            tx.commit().await.map_err(identity_err)?;
            return Ok(None);
        }
        // The row is locked until the commit: a concurrent change reads it
        // only after this one, with this one's setting.
        let row =
            sqlx::query("SELECT enabled FROM org_fork_run_approval WHERE org = $1 FOR UPDATE")
                .bind(org)
                .fetch_one(&mut *tx)
                .await
                .map_err(identity_err)?;
        let old = ForkRunApproval {
            enabled: row.get::<bool, _>("enabled"),
        };
        sqlx::query(
            "UPDATE org_fork_run_approval
             SET enabled = $2, updated_by = $3, updated_at = $4
             WHERE org = $1",
        )
        .bind(org)
        .bind(setting.enabled)
        .bind(updated_by)
        .bind(updated_at)
        .execute(&mut *tx)
        .await
        .map_err(identity_err)?;
        tx.commit().await.map_err(identity_err)?;
        Ok(Some(old))
    }
}
