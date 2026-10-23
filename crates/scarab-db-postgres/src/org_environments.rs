//! [`OrgEnvironmentStore`] on Postgres (ADR-0071, migrations 0054 and 0057):
//! the org-level Environment, its EnvironmentDeploymentSources and — read
//! here, written by the `EnvironmentGovernanceStore` — its baseline protection
//! and eligibility.
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
//!
//! A write also checks, **in its own transaction**, that every source names a
//! PluginInstallation in the Org (c-min audit F1), holding those
//! installations `FOR SHARE` until it commits — see `put_org_environment`.

use async_trait::async_trait;
use sqlx::Row;

use std::num::NonZeroU32;

use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentDeploymentSource, EnvironmentProjectEligibility,
    EnvironmentProtectionRules, OrgEnvironment, OrgEnvironmentError, OrgEnvironmentStore,
};

use crate::PostgresDb;

pub(crate) fn org_environment_store_err(e: sqlx::Error) -> OrgEnvironmentError {
    OrgEnvironmentError::Store(e.to_string())
}

/// Build an [`OrgEnvironment`] from its `org_environments` row (selected with
/// [`ORG_ENVIRONMENT_COLUMNS`]), its sources and its eligibility patterns. A
/// deadline outside the domain bounds is a row this binary cannot read —
/// refused, never clamped, so a status is never folded against a deadline
/// nobody set; likewise a rule the CHECKs should have kept out.
pub(crate) fn org_environment_from_row(
    row: &sqlx::postgres::PgRow,
    deployment_sources: Vec<EnvironmentDeploymentSource>,
    eligible_project_patterns: Vec<String>,
) -> Result<OrgEnvironment, OrgEnvironmentError> {
    let name: String = row.get("name");
    let ms: i64 = row.get("deployment_deadline_ms");
    let deployment_deadline = EnvironmentDeploymentDeadline::new(ms).map_err(|e| {
        OrgEnvironmentError::Store(format!(
            "environment `{name}` holds an unreadable deadline: {e}"
        ))
    })?;
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
        deployment_deadline,
        deployment_sources,
        protection,
        eligibility,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        updated_by: row.get("updated_by"),
    })
}

/// The five protection rules from a row carrying them as columns — an
/// `org_environments` baseline or an `environment_project_overlays` overlay
/// (one shape, ADR-0071 §1, §3). `what` names the row in an error.
pub(crate) fn protection_rules_from_row(
    row: &sqlx::postgres::PgRow,
    what: &str,
) -> Result<EnvironmentProtectionRules, OrgEnvironmentError> {
    let unreadable =
        |rule: &str| OrgEnvironmentError::Store(format!("`{what}` holds an unreadable {rule}"));
    let wait_timer_s =
        u32::try_from(row.get::<i32, _>("wait_timer_s")).map_err(|_| unreadable("wait timer"))?;
    let deployment_concurrency = match row.get::<Option<i32>, _>("deployment_concurrency") {
        None => None,
        Some(n) => Some(
            u32::try_from(n)
                .ok()
                .and_then(NonZeroU32::new)
                .ok_or_else(|| unreadable("deployment concurrency"))?,
        ),
    };
    Ok(EnvironmentProtectionRules {
        approvers: row.get("approvers"),
        allowed_refs: row.get("allowed_refs"),
        wait_timer_s,
        require_reason: row.get("require_reason"),
        deployment_concurrency,
    })
}

pub(crate) const ORG_ENVIRONMENT_COLUMNS: &str =
    "org, name, description, deployment_deadline_ms, created_at, updated_at, updated_by, \
     approvers, allowed_refs, wait_timer_s, require_reason, deployment_concurrency, \
     eligible_all_projects";

/// One Environment's sources, sorted, read with `executor`.
pub(crate) async fn deployment_sources_of<'e, E>(
    executor: E,
    org: &str,
    name: &str,
) -> Result<Vec<EnvironmentDeploymentSource>, OrgEnvironmentError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query(
        "SELECT installation, vendor_grouping FROM environment_deployment_sources
         WHERE org = $1 AND environment = $2
         ORDER BY installation, vendor_grouping",
    )
    .bind(org)
    .bind(name)
    .fetch_all(executor)
    .await
    .map_err(org_environment_store_err)?
    .iter()
    .map(|r| EnvironmentDeploymentSource {
        installation: r.get("installation"),
        vendor_grouping: r.get("vendor_grouping"),
    })
    .collect())
}

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
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(org_environment_store_err)?;
        // The upsert takes the Environment row's lock, so two concurrent
        // replaces of one Environment serialize here and the source list ends
        // up as exactly one of them wrote it. `created_at` is not in the SET:
        // a replace keeps the original. Nor are the baseline and eligibility
        // (ADR-0071 §1, §2): a create takes the column defaults (permissive,
        // no eligible Project), a replace keeps what is stored — this is the
        // c-min whole-Environment form, which knows nothing of governance and
        // must never reset it by omission.
        let stored_row = sqlx::query(&format!(
            "INSERT INTO org_environments
                 (org, name, description, deployment_deadline_ms, created_at, updated_at, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (org, name) DO UPDATE SET
                 description = EXCLUDED.description,
                 deployment_deadline_ms = EXCLUDED.deployment_deadline_ms,
                 updated_at = EXCLUDED.updated_at,
                 updated_by = EXCLUDED.updated_by
             RETURNING {ORG_ENVIRONMENT_COLUMNS}"
        ))
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
        let eligible_project_patterns =
            eligible_project_patterns_of(&mut *tx, &env.org, &env.name).await?;

        // F1 (c-min audit): every source must name a PluginInstallation in
        // this Org. Checked here, in the write's transaction, with the
        // installations named by the stored sources AND the new ones locked
        // FOR SHARE (sorted, before any source row is touched):
        // - an uninstall locks its installation row and then deletes the
        //   sources naming it, so it either finishes before this check reads
        //   (and the name is refused) or waits for this write to commit —
        //   never a source left naming a name an uninstall is retiring
        //   (red-team F-10), and never each waiting on the other's source
        //   rows (review #9: a 40P01 deadlock);
        // - a kill or an ingest (FOR UPDATE on the same row) waits for this
        //   write, which takes no lock either of them holds.
        // The stored list is read under the Environment row lock taken just
        // above, so a concurrent replace of this Environment cannot change it.
        let stored_installations: std::collections::BTreeSet<String> = sqlx::query_scalar(
            "SELECT DISTINCT installation FROM environment_deployment_sources
             WHERE org = $1 AND environment = $2",
        )
        .bind(&env.org)
        .bind(&env.name)
        .fetch_all(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        .into_iter()
        .collect();
        let mut named: std::collections::BTreeSet<&str> =
            stored_installations.iter().map(String::as_str).collect();
        named.extend(
            env.deployment_sources
                .iter()
                .map(|s| s.installation.as_str()),
        );
        let named: Vec<&str> = named.into_iter().collect();
        let installed: std::collections::BTreeSet<String> = sqlx::query_scalar(
            "SELECT name FROM plugin_installations
             WHERE org = $1 AND name = ANY($2::text[])
             ORDER BY name
             FOR SHARE",
        )
        .bind(&env.org)
        .bind(&named)
        .fetch_all(&mut *tx)
        .await
        .map_err(org_environment_store_err)?
        .into_iter()
        .collect();
        let new_installations: std::collections::BTreeSet<&str> = env
            .deployment_sources
            .iter()
            .map(|s| s.installation.as_str())
            .collect();
        if let Some(missing) = new_installations
            .into_iter()
            .find(|name| !installed.contains(*name))
        {
            // Dropping `tx` rolls the Environment upsert back too.
            return Err(OrgEnvironmentError::DeploymentSourceNotInstalled {
                installation: missing.to_string(),
                org: env.org.clone(),
                already_listed: stored_installations.contains(missing),
            });
        }

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
        org_environment_from_row(&stored_row, deployment_sources, eligible_project_patterns)
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
        let deployment_sources = deployment_sources_of(self.pool(), org, name).await?;
        let eligible_project_patterns =
            eligible_project_patterns_of(self.pool(), org, name).await?;
        org_environment_from_row(&row, deployment_sources, eligible_project_patterns).map(Some)
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
                let sources = sources_by_environment.remove(&name).unwrap_or_default();
                let patterns = patterns_by_environment.remove(&name).unwrap_or_default();
                org_environment_from_row(row, sources, patterns)
            })
            .collect()
    }

    async fn delete_org_environment(
        &self,
        org: &str,
        name: &str,
    ) -> Result<bool, OrgEnvironmentError> {
        // Its sources and eligibility go with it (ON DELETE CASCADE). Overlays
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
