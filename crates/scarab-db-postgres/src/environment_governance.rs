//! [`EnvironmentGovernanceStore`] on Postgres (ADR-0071 §2, §3, §8, migration
//! 0057): the baseline and eligibility on `org_environments`, the per-Project
//! overlays and privilege grants, and [`deploy_governance`] — the read
//! admission makes.
//!
//! [`deploy_governance`]: EnvironmentGovernanceStore::deploy_governance

use async_trait::async_trait;
use sqlx::Row;

use scarab_project::{
    DeployGovernance, DeploymentTargetHome, EnvironmentDeploymentDeadline,
    EnvironmentGovernanceStore, EnvironmentMigrationAcknowledgement, EnvironmentMigrationReport,
    EnvironmentMigrationReportProject, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProjectPrivilegeGrant, EnvironmentProjectRecord, EnvironmentProtectionRules,
    ImageGrant, OrgEnvironment, OrgEnvironmentError, ProtectionRules,
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

/// The split report lines of `org` — all of them, or only `environment`'s —
/// with each Project's legacy rules and current overlay, read inside `tx` so
/// a line, the baseline and the overlays are one snapshot.
async fn migration_report_lines(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: &str,
    environment: Option<&str>,
) -> Result<Vec<EnvironmentMigrationReport>, OrgEnvironmentError> {
    let lines = sqlx::query(
        "SELECT r.environment, r.merged_projects, r.referenced_only, r.referencing_projects,
                r.created_at, r.acknowledged_at, r.acknowledged_by,
                o.name IS NOT NULL AS environment_exists, o.approvers, o.allowed_refs,
                o.wait_timer_s, o.require_reason, o.deployment_concurrency
         FROM environment_migration_report r
         LEFT JOIN org_environments o ON o.org = r.org AND o.name = r.environment
         WHERE r.org = $1 AND ($2::text IS NULL OR r.environment = $2)
         ORDER BY r.environment",
    )
    .bind(org)
    .bind(environment)
    .fetch_all(&mut **tx)
    .await
    .map_err(org_environment_store_err)?;
    if lines.is_empty() {
        return Ok(Vec::new());
    }
    // The org's legacy rows and overlays, once: a report has a handful of
    // lines, and one query each beats a query per Project.
    let mut legacy: std::collections::HashMap<(String, String), ProtectionRules> =
        std::collections::HashMap::new();
    for row in sqlx::query(
        "SELECT name, project, protection FROM legacy_project_environments WHERE org = $1",
    )
    .bind(org)
    .fetch_all(&mut **tx)
    .await
    .map_err(org_environment_store_err)?
    {
        let name: String = row.get("name");
        let project: String = row.get("project");
        // The rows were written through the typed ADR-0037 API, so every one
        // deserialises (the golden corpus pins old shapes); one that does not
        // is named rather than silently dropped from the comparison.
        let rules: ProtectionRules =
            serde_json::from_value(row.get::<serde_json::Value, _>("protection")).map_err(|e| {
                OrgEnvironmentError::Store(format!(
                    "legacy environment {org}/{project}/{name} is unreadable: {e}"
                ))
            })?;
        legacy.insert((name, project), rules);
    }
    let mut overlays: std::collections::HashMap<(String, String), EnvironmentProjectOverlay> =
        std::collections::HashMap::new();
    for row in sqlx::query(&format!(
        "SELECT {OVERLAY_COLUMNS} FROM environment_project_overlays WHERE org = $1"
    ))
    .bind(org)
    .fetch_all(&mut **tx)
    .await
    .map_err(org_environment_store_err)?
    {
        let overlay = overlay_from_row(&row)?;
        overlays.insert(
            (overlay.environment.clone(), overlay.project.clone()),
            overlay,
        );
    }

    let mut report = Vec::with_capacity(lines.len());
    for row in &lines {
        let environment: String = row.get("environment");
        let merged_projects: Vec<String> = row.get("merged_projects");
        let referencing_projects: Vec<String> = row.get("referencing_projects");
        let baseline = if row.get::<bool, _>("environment_exists") {
            Some(protection_rules_from_row(row, &environment)?)
        } else {
            None
        };
        let acknowledged = match (
            row.get::<Option<i64>, _>("acknowledged_at"),
            row.get::<Option<String>, _>("acknowledged_by"),
        ) {
            (Some(at), Some(by)) => Some(EnvironmentMigrationAcknowledgement { at, by }),
            _ => None,
        };
        let mut names: Vec<String> = merged_projects
            .iter()
            .chain(&referencing_projects)
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        let projects = names
            .into_iter()
            .map(|project| {
                let key = (environment.clone(), project.clone());
                EnvironmentMigrationReportProject {
                    legacy: legacy.get(&key).cloned(),
                    overlay: overlays.get(&key).cloned(),
                    project,
                }
            })
            .collect();
        report.push(EnvironmentMigrationReport {
            org: org.to_string(),
            environment,
            merged_projects,
            referenced_only: row.get("referenced_only"),
            referencing_projects,
            created_at: row.get("created_at"),
            acknowledged,
            baseline,
            projects,
        });
    }
    Ok(report)
}

/// Lock and read an Environment's row inside `tx` — the lock every
/// baseline and eligibility write takes — or [`OrgEnvironmentError::NotFound`].
async fn lock_environment(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: &str,
    environment: &str,
) -> Result<sqlx::postgres::PgRow, OrgEnvironmentError> {
    sqlx::query(&format!(
        "SELECT {ORG_ENVIRONMENT_COLUMNS} FROM org_environments
         WHERE org = $1 AND name = $2
         FOR UPDATE"
    ))
    .bind(org)
    .bind(environment)
    .fetch_optional(&mut **tx)
    .await
    .map_err(org_environment_store_err)?
    .ok_or_else(|| not_found(org, environment))
}

/// Eligibility compared as a set: the store returns patterns sorted, a
/// caller may hold them in any order.
fn normalized_eligibility(e: &EnvironmentProjectEligibility) -> EnvironmentProjectEligibility {
    match e {
        EnvironmentProjectEligibility::AllProjects => EnvironmentProjectEligibility::AllProjects,
        EnvironmentProjectEligibility::Projects(p) => {
            let mut p = p.clone();
            p.sort();
            p.dedup();
            EnvironmentProjectEligibility::Projects(p)
        }
    }
}

impl PostgresDb {
    /// Why a conditional overlay or grant update matched nothing: the
    /// Environment is gone, or the row is not what the caller expected.
    async fn missed(&self, org: &str, environment: &str) -> OrgEnvironmentError {
        match sqlx::query("SELECT 1 FROM org_environments WHERE org = $1 AND name = $2")
            .bind(org)
            .bind(environment)
            .fetch_optional(self.pool())
            .await
        {
            Ok(Some(_)) => OrgEnvironmentError::changed_concurrently(),
            Ok(None) => not_found(org, environment),
            Err(e) => org_environment_store_err(e),
        }
    }
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

    async fn put_environment_protection_expecting(
        &self,
        org: &str,
        environment: &str,
        protection: &EnvironmentProtectionRules,
        expected: Option<&EnvironmentProtectionRules>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        let (wait_timer_s, deployment_concurrency) = rule_columns(protection)?;
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(org_environment_store_err)?;
        if let Some(expected) = expected {
            // ADR-0073 §3: re-read under the row lock the UPDATE below would
            // take anyway, so nothing can change between this compare and
            // the write.
            let row = lock_environment(&mut tx, org, environment).await?;
            if &protection_rules_from_row(&row, environment)? != expected {
                return Err(OrgEnvironmentError::changed_concurrently());
            }
        }
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

    async fn put_environment_eligibility_expecting(
        &self,
        org: &str,
        environment: &str,
        eligibility: &EnvironmentProjectEligibility,
        expected: Option<&EnvironmentProjectEligibility>,
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
        if let Some(expected) = expected {
            // ADR-0073 §3: the Environment row lock is the one every
            // eligibility replace takes first, so the patterns read under it
            // are the ones this write replaces.
            let row = lock_environment(&mut tx, org, environment).await?;
            let stored = stored_environment(&mut tx, &row, org, environment).await?;
            if normalized_eligibility(&stored.eligibility) != normalized_eligibility(expected) {
                return Err(OrgEnvironmentError::changed_concurrently());
            }
        }
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

    async fn put_environment_project_overlay_expecting(
        &self,
        overlay: &EnvironmentProjectOverlay,
        expected: Option<Option<&EnvironmentProjectOverlay>>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<(), OrgEnvironmentError> {
        let (wait_timer_s, deployment_concurrency) = rule_columns(&overlay.rules)?;
        match expected {
            None => {}
            // ADR-0073 §3. An absent row cannot be locked, so each expectation
            // is ONE conditional statement instead: insert-if-absent, or
            // update-if-unchanged. Either matches nothing when a concurrent
            // write got there first.
            Some(None) => {
                let inserted = sqlx::query(
                    "INSERT INTO environment_project_overlays
                         (org, environment, project, approvers, allowed_refs, wait_timer_s,
                          require_reason, deployment_concurrency, deployment_gate_deadline_ms,
                          updated_at, updated_by)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                     ON CONFLICT (org, environment, project) DO NOTHING",
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
                .map_err(|e| write_err(e, &overlay.org, &overlay.environment))?
                .rows_affected();
                return if inserted == 1 {
                    Ok(())
                } else {
                    Err(OrgEnvironmentError::changed_concurrently())
                };
            }
            Some(Some(old)) => {
                let (old_wait_timer_s, old_concurrency) = rule_columns(&old.rules)?;
                let updated = sqlx::query(
                    "UPDATE environment_project_overlays SET
                         approvers = $4, allowed_refs = $5, wait_timer_s = $6,
                         require_reason = $7, deployment_concurrency = $8,
                         deployment_gate_deadline_ms = $9, updated_at = $10, updated_by = $11
                     WHERE org = $1 AND environment = $2 AND project = $3
                       AND approvers = $12 AND allowed_refs = $13 AND wait_timer_s = $14
                       AND require_reason = $15
                       AND deployment_concurrency IS NOT DISTINCT FROM $16
                       AND deployment_gate_deadline_ms IS NOT DISTINCT FROM $17",
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
                .bind(&old.rules.approvers)
                .bind(&old.rules.allowed_refs)
                .bind(old_wait_timer_s)
                .bind(old.rules.require_reason)
                .bind(old_concurrency)
                .bind(old.deployment_gate_deadline.map(|d| d.millis()))
                .execute(self.pool())
                .await
                .map_err(org_environment_store_err)?
                .rows_affected();
                return if updated == 1 {
                    Ok(())
                } else {
                    Err(self.missed(&overlay.org, &overlay.environment).await)
                };
            }
        }
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

    async fn put_environment_project_privilege_grant_expecting(
        &self,
        grant: &EnvironmentProjectPrivilegeGrant,
        expected: Option<Option<&EnvironmentProjectPrivilegeGrant>>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<(), OrgEnvironmentError> {
        let images = serde_json::to_value(&grant.privileged_images)
            .map_err(|e| OrgEnvironmentError::Store(e.to_string()))?;
        match expected {
            None => {}
            // ADR-0073 §3, as for overlays: one conditional statement each.
            Some(None) => {
                let inserted = sqlx::query(
                    "INSERT INTO environment_project_privilege_grants
                         (org, environment, project, privileged_images, permit_k8s_overlay,
                          updated_at, updated_by)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (org, environment, project) DO NOTHING",
                )
                .bind(&grant.org)
                .bind(&grant.environment)
                .bind(&grant.project)
                .bind(&images)
                .bind(grant.permit_k8s_overlay)
                .bind(updated_at)
                .bind(updated_by)
                .execute(self.pool())
                .await
                .map_err(|e| write_err(e, &grant.org, &grant.environment))?
                .rows_affected();
                return if inserted == 1 {
                    Ok(())
                } else {
                    Err(OrgEnvironmentError::changed_concurrently())
                };
            }
            Some(Some(old)) => {
                // JSONB equality: the stored whitelist was written from the
                // same type, so equal values are equal documents.
                let old_images = serde_json::to_value(&old.privileged_images)
                    .map_err(|e| OrgEnvironmentError::Store(e.to_string()))?;
                let updated = sqlx::query(
                    "UPDATE environment_project_privilege_grants SET
                         privileged_images = $4, permit_k8s_overlay = $5,
                         updated_at = $6, updated_by = $7
                     WHERE org = $1 AND environment = $2 AND project = $3
                       AND privileged_images = $8 AND permit_k8s_overlay = $9",
                )
                .bind(&grant.org)
                .bind(&grant.environment)
                .bind(&grant.project)
                .bind(&images)
                .bind(grant.permit_k8s_overlay)
                .bind(updated_at)
                .bind(updated_by)
                .bind(&old_images)
                .bind(old.permit_k8s_overlay)
                .execute(self.pool())
                .await
                .map_err(org_environment_store_err)?
                .rows_affected();
                return if updated == 1 {
                    Ok(())
                } else {
                    Err(self.missed(&grant.org, &grant.environment).await)
                };
            }
        }
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
    async fn environment_projects(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<EnvironmentProjectRecord>, OrgEnvironmentError> {
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
            return Err(not_found(org, environment));
        };
        let env = stored_environment(&mut tx, &row, org, environment).await?;
        let overlays = sqlx::query(&format!(
            "SELECT {OVERLAY_COLUMNS} FROM environment_project_overlays
             WHERE org = $1 AND environment = $2"
        ))
        .bind(org)
        .bind(environment)
        .fetch_all(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        .iter()
        .map(overlay_from_row)
        .collect::<Result<Vec<_>, _>>()?;
        let grants = sqlx::query(&format!(
            "SELECT {GRANT_COLUMNS} FROM environment_project_privilege_grants
             WHERE org = $1 AND environment = $2"
        ))
        .bind(org)
        .bind(environment)
        .fetch_all(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        .iter()
        .map(grant_from_row)
        .collect::<Result<Vec<_>, _>>()?;
        tx.commit().await.map_err(org_environment_store_err)?;
        Ok(EnvironmentProjectRecord::collect(&env, overlays, grants))
    }

    async fn environment_migration_report(
        &self,
        org: &str,
    ) -> Result<Vec<EnvironmentMigrationReport>, OrgEnvironmentError> {
        let mut tx = snapshot(self).await?;
        let report = migration_report_lines(&mut tx, org, None).await?;
        tx.commit().await.map_err(org_environment_store_err)?;
        Ok(report)
    }

    async fn deployment_target_homes(
        &self,
        org: &str,
        targets: &[(String, String)],
    ) -> Result<Vec<DeploymentTargetHome>, OrgEnvironmentError> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let (sources, ids): (Vec<&str>, Vec<&str>) = targets
            .iter()
            .map(|(source, id)| (source.as_str(), id.as_str()))
            .unzip();
        // Current routing first (the same join `PluginStore::target_routings`
        // makes: installation → its reported target → the Environment listing
        // that target's grouping NOW), else the newest recorded input's
        // Environment (`deployment_inputs_target_idx`, migration 0058).
        let rows = sqlx::query(
            "SELECT q.source, q.target_id,
                    COALESCE(
                        (SELECT routed.environment
                         FROM plugin_installations i
                         JOIN deployment_source_targets t
                           ON t.installation_id = i.id AND t.target_id = q.target_id
                         JOIN environment_deployment_sources routed
                           ON routed.org = i.org AND routed.installation = i.name
                          AND routed.vendor_grouping = t.vendor_grouping
                         WHERE i.org = $1 AND i.name = q.source
                         LIMIT 1),
                        (SELECT d.environment
                         FROM deployment_inputs d
                         WHERE d.org = $1 AND d.target_source = q.source
                           AND d.target_id = q.target_id
                         ORDER BY d.seq DESC
                         LIMIT 1)
                    ) AS environment
             FROM UNNEST($2::text[], $3::text[]) AS q(source, target_id)",
        )
        .bind(org)
        .bind(&sources)
        .bind(&ids)
        .fetch_all(self.pool())
        .await
        .map_err(org_environment_store_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(DeploymentTargetHome {
                    source: r.get("source"),
                    id: r.get("target_id"),
                    environment: r.get::<Option<String>, _>("environment")?,
                })
            })
            .collect())
    }

    async fn acknowledge_environment_migration_report(
        &self,
        org: &str,
        environment: &str,
        acknowledged_by: &str,
        acknowledged_at: i64,
    ) -> Result<Option<EnvironmentMigrationReport>, OrgEnvironmentError> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(org_environment_store_err)?;
        // COALESCE keeps the first acknowledgement: the record answers "who
        // first looked at this", which a repeat click must not overwrite.
        let updated = sqlx::query(
            "UPDATE environment_migration_report SET
                 acknowledged_at = COALESCE(acknowledged_at, $3),
                 acknowledged_by = COALESCE(acknowledged_by, $4)
             WHERE org = $1 AND environment = $2",
        )
        .bind(org)
        .bind(environment)
        .bind(acknowledged_at)
        .bind(acknowledged_by)
        .execute(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        .rows_affected();
        if updated == 0 {
            return Ok(None);
        }
        let line = migration_report_lines(&mut tx, org, Some(environment))
            .await?
            .into_iter()
            .next();
        tx.commit().await.map_err(org_environment_store_err)?;
        Ok(line)
    }
}
