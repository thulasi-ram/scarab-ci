//! [`ForkRunApprovalStore`] on Postgres (migration 0114): each org's Fork
//! Run approval setting, keyed by [`fork_run_approval_org_key`].

use async_trait::async_trait;
use sqlx::Row;

use scarab_identity::{
    fork_run_approval_org_key, ForkRunApproval, ForkRunApprovalStore, IdentityError,
};

use crate::{identity_err, PostgresDb};

/// A stored setting. The table's CHECK keeps it to the three names, so one
/// this binary cannot read is refused, never defaulted.
fn setting(row: &sqlx::postgres::PgRow) -> Result<ForkRunApproval, IdentityError> {
    let name: String = row.get("setting");
    ForkRunApproval::parse(&name).ok_or_else(|| {
        IdentityError::Issuance(format!(
            "`org_fork_run_approval.setting` holds `{name}`, which this server cannot read"
        ))
    })
}

#[async_trait]
impl ForkRunApprovalStore for PostgresDb {
    async fn fork_run_approval(&self, org: &str) -> Result<Option<ForkRunApproval>, IdentityError> {
        let row = sqlx::query("SELECT setting FROM org_fork_run_approval WHERE org = $1")
            .bind(fork_run_approval_org_key(org))
            .fetch_optional(self.pool())
            .await
            .map_err(identity_err)?;
        row.as_ref().map(setting).transpose()
    }

    async fn put_fork_run_approval(
        &self,
        org: &str,
        expected: ForkRunApproval,
        new: ForkRunApproval,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<bool, IdentityError> {
        let org = fork_run_approval_org_key(org);
        let mut tx = self.pool().begin().await.map_err(identity_err)?;
        // The row, locked until the commit: a concurrent change reads it only
        // after this one, and finds this one's setting.
        let row =
            sqlx::query("SELECT setting FROM org_fork_run_approval WHERE org = $1 FOR UPDATE")
                .bind(org.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(identity_err)?;
        match row {
            Some(row) => {
                if setting(&row)? != expected {
                    return Ok(false);
                }
                sqlx::query(
                    "UPDATE org_fork_run_approval
                     SET setting = $2, updated_by = $3, updated_at = $4
                     WHERE org = $1",
                )
                .bind(org.as_str())
                .bind(new.as_str())
                .bind(updated_by)
                .bind(updated_at)
                .execute(&mut *tx)
                .await
                .map_err(identity_err)?;
            }
            None => {
                // No row: the org has the default.
                if ForkRunApproval::default() != expected {
                    return Ok(false);
                }
                // A concurrent first insert wins; this change then lost the
                // race, and nothing is written.
                let inserted = sqlx::query(
                    "INSERT INTO org_fork_run_approval (org, setting, updated_by, updated_at)
                     VALUES ($1, $2, $3, $4)
                     ON CONFLICT (org) DO NOTHING",
                )
                .bind(org.as_str())
                .bind(new.as_str())
                .bind(updated_by)
                .bind(updated_at)
                .execute(&mut *tx)
                .await
                .map_err(identity_err)?
                .rows_affected();
                if inserted == 0 {
                    return Ok(false);
                }
            }
        }
        tx.commit().await.map_err(identity_err)?;
        Ok(true)
    }
}
