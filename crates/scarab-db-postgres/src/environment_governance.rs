//! [`EnvironmentGovernanceStore`] on Postgres (ADR-0071 §2, §3, §8, migration
//! 0057): the baseline and eligibility on `org_environments`, the per-Project
//! overlays and privilege grants, and [`deploy_governance`] — the read
//! admission makes.
//!
//! [`deploy_governance`]: EnvironmentGovernanceStore::deploy_governance

use async_trait::async_trait;
use sqlx::Row;

use scarab_project::{
    DeployGovernance, EnvironmentDeploymentDeadline, EnvironmentGovernanceStore,
    EnvironmentProjectEligibility, EnvironmentProjectOverlay, EnvironmentProjectPrivilegeGrant,
    EnvironmentProtectionRules, ImageGrant, OrgEnvironment, OrgEnvironmentError,
};

use crate::org_environments::{
    deployment_sources_of, eligible_project_patterns_of, org_environment_from_row,
    org_environment_store_err, protection_rules_from_row, ORG_ENVIRONMENT_COLUMNS,
};
use crate::PostgresDb;

/// Postgres' `foreign_key_violation`: a governance write naming an
/// Environment that does not exist.
const FOREIGN_KEY_VIOLATION: &str = "23503";

fn not_found(org: &str, environment: &str) -> OrgEnvironmentError {
    OrgEnvironmentError::NotFound {
        org: org.to_string(),
        environment: environment.to_string(),
    }
}

/// A write error, with a missing parent Environment named as such.
fn write_err(e: sqlx::Error, org: &str, environment: &str) -> OrgEnvironmentError {
    match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some(FOREIGN_KEY_VIOLATION) => {
            not_found(org, environment)
        }
        _ => org_environment_store_err(e),
    }
}

/// The two rules whose Rust type is wider or narrower than their column, as
/// the columns hold them. A wait timer above `i32::MAX` seconds (68 years)
/// is refused rather than clamped: clamping would store a rule nobody set.
fn rule_columns(
    rules: &EnvironmentProtectionRules,
) -> Result<(i32, Option<i32>), OrgEnvironmentError> {
    let wait_timer_s = i32::try_from(rules.wait_timer_s).map_err(|_| {
        OrgEnvironmentError::Invalid(format!("wait timer must be at most {} seconds", i32::MAX))
    })?;
    let deployment_concurrency = rules
        .deployment_concurrency
        .map(|n| {
            i32::try_from(n.get()).map_err(|_| {
                OrgEnvironmentError::Invalid(format!(
                    "deployment concurrency must be at most {}",
                    i32::MAX
                ))
            })
        })
        .transpose()?;
    Ok((wait_timer_s, deployment_concurrency))
}

const OVERLAY_COLUMNS: &str = "org, environment, project, approvers, allowed_refs, \
     wait_timer_s, require_reason, deployment_concurrency, deployment_gate_deadline_ms";

fn overlay_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<EnvironmentProjectOverlay, OrgEnvironmentError> {
    let org: String = row.get("org");
    let environment: String = row.get("environment");
    let project: String = row.get("project");
    let what = format!("overlay {org}/{environment}/{project}");
    let rules = protection_rules_from_row(row, &what)?;
    let deployment_gate_deadline = row
        .get::<Option<i64>, _>("deployment_gate_deadline_ms")
        .map(|ms| {
            EnvironmentDeploymentDeadline::new(ms).map_err(|e| {
                OrgEnvironmentError::Store(format!("`{what}` holds an unreadable deadline: {e}"))
            })
        })
        .transpose()?;
    Ok(EnvironmentProjectOverlay {
        org,
        environment,
        project,
        rules,
        deployment_gate_deadline,
    })
}

const GRANT_COLUMNS: &str = "org, environment, project, privileged_images, permit_k8s_overlay";

fn grant_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<EnvironmentProjectPrivilegeGrant, OrgEnvironmentError> {
    let org: String = row.get("org");
    let environment: String = row.get("environment");
    let project: String = row.get("project");
    let privileged_images: Vec<ImageGrant> = serde_json::from_value(
        row.get::<serde_json::Value, _>("privileged_images"),
    )
    .map_err(|e| {
        OrgEnvironmentError::Store(format!(
            "privilege grant {org}/{environment}/{project} holds an unreadable \
                     whitelist: {e}"
        ))
    })?;
    Ok(EnvironmentProjectPrivilegeGrant {
        org,
        environment,
        project,
        privileged_images,
        permit_k8s_overlay: row.get("permit_k8s_overlay"),
    })
}

/// Begin a read-only transaction that sees **one snapshot** for every
/// statement. Under the default READ COMMITTED each statement gets its own,
/// so a governance read racing an admin's eligibility-then-overlay edit could
/// pair the old eligibility with the new overlay — a decision neither state
/// would make. REPEATABLE READ makes the several reads one read.
async fn snapshot(
    db: &PostgresDb,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, OrgEnvironmentError> {
    let mut tx = db.pool().begin().await.map_err(org_environment_store_err)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(org_environment_store_err)?;
    Ok(tx)
}

/// The stored Environment, re-read inside `tx` after a governance write so
/// the caller gets what is now stored.
async fn stored_environment(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: &sqlx::postgres::PgRow,
    org: &str,
    environment: &str,
) -> Result<OrgEnvironment, OrgEnvironmentError> {
    let sources = deployment_sources_of(&mut **tx, org, environment).await?;
    let patterns = eligible_project_patterns_of(&mut **tx, org, environment).await?;
    org_environment_from_row(row, sources, patterns)
}

#[async_trait]
impl EnvironmentGovernanceStore for PostgresDb {
    async fn deploy_governance(
        &self,
        org: &str,
        project: &str,
        environment: &str,
    ) -> Result<Option<DeployGovernance>, OrgEnvironmentError> {
        let mut tx = snapshot(self).await?;
        let Some(row) = sqlx::query(&format!(
            "SELECT {ORG_ENVIRONMENT_COLUMNS} FROM org_environments WHERE org = $1 AND name = $2"
        ))
        .bind(org)
        .bind(environment)
        .fetch_optional(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        else {
            return Ok(None);
        };
        let env = stored_environment(&mut tx, &row, org, environment).await?;
        let overlay = sqlx::query(&format!(
            "SELECT {OVERLAY_COLUMNS} FROM environment_project_overlays
             WHERE org = $1 AND environment = $2 AND project = $3"
        ))
        .bind(org)
        .bind(environment)
        .bind(project)
        .fetch_optional(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        .as_ref()
        .map(overlay_from_row)
        .transpose()?;
        let grant = sqlx::query(&format!(
            "SELECT {GRANT_COLUMNS} FROM environment_project_privilege_grants
             WHERE org = $1 AND environment = $2 AND project = $3"
        ))
        .bind(org)
        .bind(environment)
        .bind(project)
        .fetch_optional(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        .as_ref()
        .map(grant_from_row)
        .transpose()?;
        tx.commit().await.map_err(org_environment_store_err)?;
        Ok(Some(DeployGovernance::new(env, project, overlay, grant)))
    }

    async fn project_environment_governance(
        &self,
        org: &str,
        project: &str,
    ) -> Result<Vec<DeployGovernance>, OrgEnvironmentError> {
        let mut tx = snapshot(self).await?;
        let rows = sqlx::query(&format!(
            "SELECT {ORG_ENVIRONMENT_COLUMNS} FROM org_environments WHERE org = $1 ORDER BY name"
        ))
        .bind(org)
        .fetch_all(&mut *tx)
        .await
        .map_err(org_environment_store_err)?;
        let mut overlays: std::collections::HashMap<String, EnvironmentProjectOverlay> =
            std::collections::HashMap::new();
        for row in sqlx::query(&format!(
            "SELECT {OVERLAY_COLUMNS} FROM environment_project_overlays
             WHERE org = $1 AND project = $2"
        ))
        .bind(org)
        .bind(project)
        .fetch_all(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        {
            let overlay = overlay_from_row(&row)?;
            overlays.insert(overlay.environment.clone(), overlay);
        }
        let mut grants: std::collections::HashMap<String, EnvironmentProjectPrivilegeGrant> =
            std::collections::HashMap::new();
        for row in sqlx::query(&format!(
            "SELECT {GRANT_COLUMNS} FROM environment_project_privilege_grants
             WHERE org = $1 AND project = $2"
        ))
        .bind(org)
        .bind(project)
        .fetch_all(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        {
            let grant = grant_from_row(&row)?;
            grants.insert(grant.environment.clone(), grant);
        }
        let mut governance = Vec::new();
        for row in &rows {
            let name: String = row.get("name");
            let env = stored_environment(&mut tx, row, org, &name).await?;
            let g =
                DeployGovernance::new(env, project, overlays.remove(&name), grants.remove(&name));
            // Eligibility is a glob the store cannot evaluate in SQL without
            // a second matcher, so the filter runs here, on the one matcher.
            if g.eligible || g.overlay.is_some() || g.privilege_grant.is_some() {
                governance.push(g);
            }
        }
        tx.commit().await.map_err(org_environment_store_err)?;
        Ok(governance)
    }

    async fn put_environment_protection(
        &self,
        org: &str,
        environment: &str,
        protection: &EnvironmentProtectionRules,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        let (wait_timer_s, deployment_concurrency) = rule_columns(protection)?;
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(org_environment_store_err)?;
        let row = sqlx::query(&format!(
            "UPDATE org_environments SET
                 approvers = $3, allowed_refs = $4, wait_timer_s = $5, require_reason = $6,
                 deployment_concurrency = $7, updated_at = $8, updated_by = $9
             WHERE org = $1 AND name = $2
             RETURNING {ORG_ENVIRONMENT_COLUMNS}"
        ))
        .bind(org)
        .bind(environment)
        .bind(&protection.approvers)
        .bind(&protection.allowed_refs)
        .bind(wait_timer_s)
        .bind(protection.require_reason)
        .bind(deployment_concurrency)
        .bind(updated_at)
        .bind(updated_by)
        .fetch_optional(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        .ok_or_else(|| not_found(org, environment))?;
        let stored = stored_environment(&mut tx, &row, org, environment).await?;
        tx.commit().await.map_err(org_environment_store_err)?;
        Ok(stored)
    }

    async fn put_environment_eligibility(
        &self,
        org: &str,
        environment: &str,
        eligibility: &EnvironmentProjectEligibility,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        eligibility.validate()?;
        let (all, patterns): (bool, &[String]) = match eligibility {
            EnvironmentProjectEligibility::AllProjects => (true, &[]),
            EnvironmentProjectEligibility::Projects(p) => (false, p),
        };
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(org_environment_store_err)?;
        // The row update takes the Environment's lock first, so two
        // concurrent replaces serialize and the list ends up as one of them
        // wrote it.
        let row = sqlx::query(&format!(
            "UPDATE org_environments SET eligible_all_projects = $3, updated_at = $4, updated_by = $5
             WHERE org = $1 AND name = $2
             RETURNING {ORG_ENVIRONMENT_COLUMNS}"
        ))
        .bind(org)
        .bind(environment)
        .bind(all)
        .bind(updated_at)
        .bind(updated_by)
        .fetch_optional(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        .ok_or_else(|| not_found(org, environment))?;
        sqlx::query(
            "DELETE FROM environment_project_eligibility WHERE org = $1 AND environment = $2",
        )
        .bind(org)
        .bind(environment)
        .execute(&mut *tx)
        .await
        .map_err(org_environment_store_err)?;
        sqlx::query(
            "INSERT INTO environment_project_eligibility (org, environment, project_pattern)
             SELECT $1, $2, p FROM unnest($3::text[]) AS p",
        )
        .bind(org)
        .bind(environment)
        .bind(patterns)
        .execute(&mut *tx)
        .await
        .map_err(org_environment_store_err)?;
        let stored = stored_environment(&mut tx, &row, org, environment).await?;
        tx.commit().await.map_err(org_environment_store_err)?;
        Ok(stored)
    }

    async fn put_environment_project_overlay(
        &self,
        overlay: &EnvironmentProjectOverlay,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<(), OrgEnvironmentError> {
        let (wait_timer_s, deployment_concurrency) = rule_columns(&overlay.rules)?;
        sqlx::query(
            "INSERT INTO environment_project_overlays
                 (org, environment, project, approvers, allowed_refs, wait_timer_s, require_reason,
                  deployment_concurrency, deployment_gate_deadline_ms, updated_at, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (org, environment, project) DO UPDATE SET
                 approvers = EXCLUDED.approvers,
                 allowed_refs = EXCLUDED.allowed_refs,
                 wait_timer_s = EXCLUDED.wait_timer_s,
                 require_reason = EXCLUDED.require_reason,
                 deployment_concurrency = EXCLUDED.deployment_concurrency,
                 deployment_gate_deadline_ms = EXCLUDED.deployment_gate_deadline_ms,
                 updated_at = EXCLUDED.updated_at,
                 updated_by = EXCLUDED.updated_by",
        )
        .bind(&overlay.org)
        .bind(&overlay.environment)
        .bind(&overlay.project)
        .bind(&overlay.rules.approvers)
        .bind(&overlay.rules.allowed_refs)
        .bind(wait_timer_s)
        .bind(overlay.rules.require_reason)
        .bind(deployment_concurrency)
        .bind(overlay.deployment_gate_deadline.map(|d| d.millis()))
        .bind(updated_at)
        .bind(updated_by)
        .execute(self.pool())
        .await
        .map_err(|e| write_err(e, &overlay.org, &overlay.environment))?;
        Ok(())
    }

    async fn delete_environment_project_overlay(
        &self,
        org: &str,
        environment: &str,
        project: &str,
    ) -> Result<bool, OrgEnvironmentError> {
        let deleted = sqlx::query(
            "DELETE FROM environment_project_overlays
             WHERE org = $1 AND environment = $2 AND project = $3",
        )
        .bind(org)
        .bind(environment)
        .bind(project)
        .execute(self.pool())
        .await
        .map_err(org_environment_store_err)?
        .rows_affected();
        Ok(deleted > 0)
    }

    async fn put_environment_project_privilege_grant(
        &self,
        grant: &EnvironmentProjectPrivilegeGrant,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<(), OrgEnvironmentError> {
        let images = serde_json::to_value(&grant.privileged_images)
            .map_err(|e| OrgEnvironmentError::Store(e.to_string()))?;
        sqlx::query(
            "INSERT INTO environment_project_privilege_grants
                 (org, environment, project, privileged_images, permit_k8s_overlay,
                  updated_at, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (org, environment, project) DO UPDATE SET
                 privileged_images = EXCLUDED.privileged_images,
                 permit_k8s_overlay = EXCLUDED.permit_k8s_overlay,
                 updated_at = EXCLUDED.updated_at,
                 updated_by = EXCLUDED.updated_by",
        )
        .bind(&grant.org)
        .bind(&grant.environment)
        .bind(&grant.project)
        .bind(images)
        .bind(grant.permit_k8s_overlay)
        .bind(updated_at)
        .bind(updated_by)
        .execute(self.pool())
        .await
        .map_err(|e| write_err(e, &grant.org, &grant.environment))?;
        Ok(())
    }

    async fn delete_environment_project_privilege_grant(
        &self,
        org: &str,
        environment: &str,
        project: &str,
    ) -> Result<bool, OrgEnvironmentError> {
        let deleted = sqlx::query(
            "DELETE FROM environment_project_privilege_grants
             WHERE org = $1 AND environment = $2 AND project = $3",
        )
        .bind(org)
        .bind(environment)
        .bind(project)
        .execute(self.pool())
        .await
        .map_err(org_environment_store_err)?
        .rows_affected();
        Ok(deleted > 0)
    }
}
