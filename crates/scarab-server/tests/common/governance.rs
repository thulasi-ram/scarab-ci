//! Seed org-level Environment governance (ADR-0071) on real Postgres the way
//! migration 0057 leaves a legacy per-Project environment, so a test written
//! against the per-Project `ProtectionRules` is ported by changing its setup
//! only — never its assertions.
#![allow(dead_code)] // each test binary uses its own subset

use scarab_db_postgres::PostgresDb;
use scarab_project::{
    migrate_legacy_environment, Environment, EnvironmentDeploymentDeadline,
    EnvironmentGovernanceStore, EnvironmentProjectEligibility, OrgEnvironment, OrgEnvironmentStore,
    ProtectionRules,
};

/// Make `project`'s legacy environment `name` (with `protection`) what ADR-0071
/// §10 makes of it: the org Environment `name` (created with the permissive
/// baseline and the default deadline if missing — step 1), `project` added to
/// its eligibility by exact name (step 6), and `project`'s overlay and grant
/// from [`migrate_legacy_environment`] (steps 2, 3, 5) — replacing any it had.
pub async fn seed_migrated_environment(
    pg: &PostgresDb,
    org: &str,
    project: &str,
    name: &str,
    protection: ProtectionRules,
) {
    let existing = pg.get_org_environment(org, name).await.unwrap();
    let env = match existing {
        Some(env) => env,
        None => pg
            .put_org_environment(&OrgEnvironment {
                org: org.into(),
                name: name.into(),
                description: String::new(),
                deployment_deadline: EnvironmentDeploymentDeadline::new(
                    scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS,
                )
                .unwrap(),
                deployment_sources: Vec::new(),
                protection: Default::default(),
                eligibility: Default::default(),
                created_at: 0,
                updated_at: 0,
                updated_by: "migration:adr-0071".into(),
            })
            .await
            .unwrap(),
    };
    if let EnvironmentProjectEligibility::Projects(mut projects) = env.eligibility {
        if !projects.iter().any(|p| p == project) {
            projects.push(project.into());
            pg.put_environment_eligibility(
                org,
                name,
                &EnvironmentProjectEligibility::Projects(projects),
                "migration:adr-0071",
                0,
            )
            .await
            .unwrap();
        }
    }
    let legacy = Environment {
        name: name.into(),
        protection,
    };
    let (overlay, grant) = migrate_legacy_environment(org, project, &legacy);
    pg.put_environment_project_overlay(&overlay, "migration:adr-0071", 0)
        .await
        .unwrap();
    match grant {
        Some(grant) => pg
            .put_environment_project_privilege_grant(&grant, "migration:adr-0071", 0)
            .await
            .unwrap(),
        None => {
            pg.delete_environment_project_privilege_grant(org, name, project)
                .await
                .unwrap();
        }
    }
}
