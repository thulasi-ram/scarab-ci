//! [`OrgEnvironmentStore`] on Postgres (ADR-0071, migrations 0054 and 0057):
//! the org-level Environment and — read here, written by the
//! `EnvironmentGovernanceStore` — its baseline protection and eligibility.

use async_trait::async_trait;
use sqlx::Row;

use scarab_project::{
    EnvironmentProjectEligibility, EnvironmentProtectionRules, OrgEnvironment, OrgEnvironmentError,
    OrgEnvironmentStore,
};

use crate::PostgresDb;

pub(crate) fn org_environment_store_err(e: sqlx::Error) -> OrgEnvironmentError {
    OrgEnvironmentError::Store(e.to_string())
}

/// Build an [`OrgEnvironment`] from its `org_environments` row (selected with
/// [`ORG_ENVIRONMENT_COLUMNS`]) and its eligibility patterns. A rule the
/// CHECKs should have kept out is a row this binary cannot read — refused,
/// never clamped.
pub(crate) fn org_environment_from_row(
    row: &sqlx::postgres::PgRow,
    eligible_project_patterns: Vec<String>,
) -> Result<OrgEnvironment, OrgEnvironmentError> {
    let name: String = row.get("name");
    let protection = protection_rules_from_row(row, &name)?;
    // The flag wins over any list: the eligibility write clears the list
    // when it sets the flag, so both present is a hand-edited row, and
    // reading it as "all" is what the flag literally says.
    let eligibility = if row.get::<bool, _>("eligible_all_projects") {
        EnvironmentProjectEligibility::AllProjects
    } else {
        EnvironmentProjectEligibility::Projects(eligible_project_patterns)
    };
    Ok(OrgEnvironment {
        org: row.get("org"),
        name,
        description: row.get("description"),
        protection,
        eligibility,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        updated_by: row.get("updated_by"),
    })
}

/// The four protection rules from a row carrying them as columns — an
/// `org_environments` baseline or an `environment_project_overlays` overlay
/// (one shape, ADR-0071 §1, §3). `what` names the row in an error.
pub(crate) fn protection_rules_from_row(
    row: &sqlx::postgres::PgRow,
    what: &str,
) -> Result<EnvironmentProtectionRules, OrgEnvironmentError> {
    let wait_timer_s = u32::try_from(row.get::<i32, _>("wait_timer_s")).map_err(|_| {
        OrgEnvironmentError::Store(format!("`{what}` holds an unreadable wait timer"))
    })?;
    Ok(EnvironmentProtectionRules {
        approvers: row.get("approvers"),
        allowed_refs: row.get("allowed_refs"),
        wait_timer_s,
        require_reason: row.get("require_reason"),
    })
}

pub(crate) const ORG_ENVIRONMENT_COLUMNS: &str =
    "org, name, description, created_at, updated_at, updated_by, \
     approvers, allowed_refs, wait_timer_s, require_reason, eligible_all_projects";

/// One Environment's eligible project patterns, sorted, read with `executor`.
pub(crate) async fn eligible_project_patterns_of<'e, E>(
    executor: E,
    org: &str,
    name: &str,
) -> Result<Vec<String>, OrgEnvironmentError>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_scalar(
        "SELECT project_pattern FROM environment_project_eligibility
         WHERE org = $1 AND environment = $2
         ORDER BY project_pattern",
    )
    .bind(org)
    .bind(name)
    .fetch_all(executor)
    .await
    .map_err(org_environment_store_err)
}

#[async_trait]
impl OrgEnvironmentStore for PostgresDb {
    async fn put_org_environment(
        &self,
        env: &OrgEnvironment,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        // The server validates too; checking here keeps the store's contract
        // ("refuses an invalid env") true for every caller.
        env.validate()?;
        // `created_at` is not in the SET: a replace keeps the original. Nor
        // are the baseline and eligibility (ADR-0071 §1, §2): a create takes
        // the column defaults (permissive, no eligible Project), a replace
        // keeps what is stored — this is the c-min whole-Environment form,
        // which knows nothing of governance and must never reset it by
        // omission.
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(org_environment_store_err)?;
        let stored_row = sqlx::query(&format!(
            "INSERT INTO org_environments
                 (org, name, description, created_at, updated_at, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (org, name) DO UPDATE SET
                 description = EXCLUDED.description,
                 updated_at = EXCLUDED.updated_at,
                 updated_by = EXCLUDED.updated_by
             RETURNING {ORG_ENVIRONMENT_COLUMNS}"
        ))
        .bind(&env.org)
        .bind(&env.name)
        .bind(&env.description)
        .bind(env.created_at)
        .bind(env.updated_at)
        .bind(&env.updated_by)
        .fetch_one(&mut *tx)
        .await
        .map_err(org_environment_store_err)?;
        let eligible_project_patterns =
            eligible_project_patterns_of(&mut *tx, &env.org, &env.name).await?;
        tx.commit().await.map_err(org_environment_store_err)?;
        org_environment_from_row(&stored_row, eligible_project_patterns)
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
        let eligible_project_patterns =
            eligible_project_patterns_of(self.pool(), org, name).await?;
        org_environment_from_row(&row, eligible_project_patterns).map(Some)
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
        let mut patterns_by_environment: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for r in sqlx::query(
            "SELECT environment, project_pattern FROM environment_project_eligibility
             WHERE org = $1
             ORDER BY environment, project_pattern",
        )
        .bind(org)
        .fetch_all(self.pool())
        .await
        .map_err(org_environment_store_err)?
        {
            patterns_by_environment
                .entry(r.get("environment"))
                .or_default()
                .push(r.get("project_pattern"));
        }
        rows.iter()
            .map(|row| {
                let name: String = row.get("name");
                let patterns = patterns_by_environment.remove(&name).unwrap_or_default();
                org_environment_from_row(row, patterns)
            })
            .collect()
    }

    async fn delete_org_environment(
        &self,
        org: &str,
        name: &str,
    ) -> Result<bool, OrgEnvironmentError> {
        // Its eligibility goes with it (ON DELETE CASCADE). Overlays
        // and grants do NOT (ON DELETE RESTRICT, ADR-0071 §3, §8): a Project's
        // stricter rules and its privileges are never dropped as a side
        // effect, so the delete is refused while any exist. The pre-check
        // names them; the RESTRICT is what holds against a racing write.
        let holders: Vec<String> = sqlx::query_scalar(
            "SELECT project FROM environment_project_overlays WHERE org = $1 AND environment = $2
             UNION
             SELECT project FROM environment_project_privilege_grants
             WHERE org = $1 AND environment = $2
             ORDER BY 1",
        )
        .bind(org)
        .bind(name)
        .fetch_all(self.pool())
        .await
        .map_err(org_environment_store_err)?;
        let in_use = |projects: &[String]| {
            OrgEnvironmentError::InUse(format!(
                "environment `{name}` still has project overlays or privilege grants \
                 ({}); remove them first",
                projects.join(", ")
            ))
        };
        if !holders.is_empty() {
            return Err(in_use(&holders));
        }
        let deleted = sqlx::query("DELETE FROM org_environments WHERE org = $1 AND name = $2")
            .bind(org)
            .bind(name)
            .execute(self.pool())
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(d) if d.code().as_deref() == Some("23503") => {
                    in_use(&["written concurrently".to_string()])
                }
                _ => org_environment_store_err(e),
            })?
            .rows_affected();
        Ok(deleted > 0)
    }
}
