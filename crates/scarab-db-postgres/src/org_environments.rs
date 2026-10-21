//! [`OrgEnvironmentStore`] on Postgres (ADR-0071, migration 0054): the
//! minimal org-level Environment and its EnvironmentDeploymentSources.
//!
//! A **write** is atomic: it replaces an Environment's row **and its whole
//! source list in one transaction**, so no edit is ever half-stored, and a
//! source is never briefly routed nowhere (or to two places) while an admin
//! moves it — the routing lookup is a single statement. **Reads are not**:
//! `get` and `list` read the row and its sources in two statements, so one
//! racing a replace can pair the old description and deadline with the new
//! sources. Nothing acts on that pairing — the Deployment fold reads only the
//! deadline — so the skew is accepted rather than paid for with a
//! transaction per read.
//! The `(org, installation, vendor_grouping)` primary key is what keeps a
//! source in exactly one Environment; this module only turns a collision into
//! [`OrgEnvironmentError::DeploymentSourceTaken`] naming the holder.

use async_trait::async_trait;
use sqlx::Row;

use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentDeploymentSource, OrgEnvironment,
    OrgEnvironmentError, OrgEnvironmentStore,
};

use crate::PostgresDb;

fn org_environment_store_err(e: sqlx::Error) -> OrgEnvironmentError {
    OrgEnvironmentError::Store(e.to_string())
}

/// Build an [`OrgEnvironment`] from its `org_environments` row and its
/// sources. A deadline outside the domain bounds is a row this binary cannot
/// read — refused, never clamped, so a status is never folded against a
/// deadline nobody set.
fn org_environment_from_row(
    row: &sqlx::postgres::PgRow,
    deployment_sources: Vec<EnvironmentDeploymentSource>,
) -> Result<OrgEnvironment, OrgEnvironmentError> {
    let name: String = row.get("name");
    let ms: i64 = row.get("deployment_deadline_ms");
    let deployment_deadline = EnvironmentDeploymentDeadline::new(ms).map_err(|e| {
        OrgEnvironmentError::Store(format!(
            "environment `{name}` holds an unreadable deadline: {e}"
        ))
    })?;
    Ok(OrgEnvironment {
        org: row.get("org"),
        name,
        description: row.get("description"),
        deployment_deadline,
        deployment_sources,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        updated_by: row.get("updated_by"),
    })
}

const ORG_ENVIRONMENT_COLUMNS: &str =
    "org, name, description, deployment_deadline_ms, created_at, updated_at, updated_by";

#[async_trait]
impl OrgEnvironmentStore for PostgresDb {
    async fn put_org_environment(
        &self,
        env: &OrgEnvironment,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        // The server validates too; checking here keeps the store's contract
        // ("refuses an invalid env") true for every caller.
        env.validate()?;
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(org_environment_store_err)?;
        // The upsert takes the Environment row's lock, so two concurrent
        // replaces of one Environment serialize here and the source list ends
        // up as exactly one of them wrote it. `created_at` is not in the SET:
        // a replace keeps the original.
        let created_at: i64 = sqlx::query_scalar(
            "INSERT INTO org_environments
                 (org, name, description, deployment_deadline_ms, created_at, updated_at, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (org, name) DO UPDATE SET
                 description = EXCLUDED.description,
                 deployment_deadline_ms = EXCLUDED.deployment_deadline_ms,
                 updated_at = EXCLUDED.updated_at,
                 updated_by = EXCLUDED.updated_by
             RETURNING created_at",
        )
        .bind(&env.org)
        .bind(&env.name)
        .bind(&env.description)
        .bind(env.deployment_deadline.millis())
        .bind(env.created_at)
        .bind(env.updated_at)
        .bind(&env.updated_by)
        .fetch_one(&mut *tx)
        .await
        .map_err(org_environment_store_err)?;

        sqlx::query(
            "DELETE FROM environment_deployment_sources WHERE org = $1 AND environment = $2",
        )
        .bind(&env.org)
        .bind(&env.name)
        .execute(&mut *tx)
        .await
        .map_err(org_environment_store_err)?;

        let mut deployment_sources = env.deployment_sources.clone();
        deployment_sources.sort();
        for source in &deployment_sources {
            // `DO NOTHING` rather than catching 23505: a failed statement
            // aborts the transaction, and naming the holder needs one more
            // read in it. If the colliding row is another transaction's,
            // still uncommitted, Postgres waits for it to settle first.
            let inserted = sqlx::query(
                "INSERT INTO environment_deployment_sources
                     (org, environment, installation, vendor_grouping)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (org, installation, vendor_grouping) DO NOTHING",
            )
            .bind(&env.org)
            .bind(&env.name)
            .bind(&source.installation)
            .bind(&source.vendor_grouping)
            .execute(&mut *tx)
            .await
            .map_err(org_environment_store_err)?
            .rows_affected();
            if inserted == 1 {
                continue;
            }
            // This Environment's own rows were deleted above and `validate`
            // refused duplicates, so the holder is another Environment.
            let holder: Option<String> = sqlx::query_scalar(
                "SELECT environment FROM environment_deployment_sources
                 WHERE org = $1 AND installation = $2 AND vendor_grouping = $3",
            )
            .bind(&env.org)
            .bind(&source.installation)
            .bind(&source.vendor_grouping)
            .fetch_optional(&mut *tx)
            .await
            .map_err(org_environment_store_err)?;
            // Dropping `tx` rolls the whole replace back.
            return Err(match holder {
                Some(environment) => OrgEnvironmentError::DeploymentSourceTaken {
                    installation: source.installation.clone(),
                    vendor_grouping: source.vendor_grouping.clone(),
                    environment,
                },
                None => OrgEnvironmentError::Store(format!(
                    "deployment source `{}` / `{}` changed hands mid-write; retry",
                    source.installation, source.vendor_grouping
                )),
            });
        }
        tx.commit().await.map_err(org_environment_store_err)?;
        Ok(OrgEnvironment {
            created_at,
            deployment_sources,
            ..env.clone()
        })
    }

    async fn get_org_environment(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Option<OrgEnvironment>, OrgEnvironmentError> {
        let sql = format!(
            "SELECT {ORG_ENVIRONMENT_COLUMNS} FROM org_environments WHERE org = $1 AND name = $2"
        );
        let Some(row) = sqlx::query(&sql)
            .bind(org)
            .bind(name)
            .fetch_optional(self.pool())
            .await
            .map_err(org_environment_store_err)?
        else {
            return Ok(None);
        };
        let deployment_sources = sqlx::query(
            "SELECT installation, vendor_grouping FROM environment_deployment_sources
             WHERE org = $1 AND environment = $2
             ORDER BY installation, vendor_grouping",
        )
        .bind(org)
        .bind(name)
        .fetch_all(self.pool())
        .await
        .map_err(org_environment_store_err)?
        .iter()
        .map(|r| EnvironmentDeploymentSource {
            installation: r.get("installation"),
            vendor_grouping: r.get("vendor_grouping"),
        })
        .collect();
        org_environment_from_row(&row, deployment_sources).map(Some)
    }

    async fn list_org_environments(
        &self,
        org: &str,
    ) -> Result<Vec<OrgEnvironment>, OrgEnvironmentError> {
        let sql = format!(
            "SELECT {ORG_ENVIRONMENT_COLUMNS} FROM org_environments WHERE org = $1 ORDER BY name"
        );
        let rows = sqlx::query(&sql)
            .bind(org)
            .fetch_all(self.pool())
            .await
            .map_err(org_environment_store_err)?;
        // Every source in the Org in one query, grouped by Environment.
        let mut sources_by_environment: std::collections::HashMap<
            String,
            Vec<EnvironmentDeploymentSource>,
        > = std::collections::HashMap::new();
        for r in sqlx::query(
            "SELECT environment, installation, vendor_grouping FROM environment_deployment_sources
             WHERE org = $1
             ORDER BY environment, installation, vendor_grouping",
        )
        .bind(org)
        .fetch_all(self.pool())
        .await
        .map_err(org_environment_store_err)?
        {
            sources_by_environment
                .entry(r.get("environment"))
                .or_default()
                .push(EnvironmentDeploymentSource {
                    installation: r.get("installation"),
                    vendor_grouping: r.get("vendor_grouping"),
                });
        }
        rows.iter()
            .map(|row| {
                let name: String = row.get("name");
                let sources = sources_by_environment.remove(&name).unwrap_or_default();
                org_environment_from_row(row, sources)
            })
            .collect()
    }

    async fn delete_org_environment(
        &self,
        org: &str,
        name: &str,
    ) -> Result<bool, OrgEnvironmentError> {
        // Its sources go with it (ON DELETE CASCADE).
        let deleted = sqlx::query("DELETE FROM org_environments WHERE org = $1 AND name = $2")
            .bind(org)
            .bind(name)
            .execute(self.pool())
            .await
            .map_err(org_environment_store_err)?
            .rows_affected();
        Ok(deleted > 0)
    }

    async fn environment_for_deployment_source(
        &self,
        org: &str,
        installation: &str,
        vendor_grouping: &str,
    ) -> Result<Option<String>, OrgEnvironmentError> {
        sqlx::query_scalar(
            "SELECT environment FROM environment_deployment_sources
             WHERE org = $1 AND installation = $2 AND vendor_grouping = $3",
        )
        .bind(org)
        .bind(installation)
        .bind(vendor_grouping)
        .fetch_optional(self.pool())
        .await
        .map_err(org_environment_store_err)
    }
}
