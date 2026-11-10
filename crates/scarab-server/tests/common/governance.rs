//! Seed org-level Environment governance (ADR-0071) on real Postgres: one
//! Project's overlay over an Environment's baseline, with the Project
//! eligible for it.
#![allow(dead_code)] // each test binary uses its own subset

use scarab_db_postgres::PostgresDb;
use scarab_project::{
    EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProtectionRules, OrgEnvironment, OrgEnvironmentStore,
};

/// Give `project` the overlay `rules` over Environment `name`, replacing any
/// it had. The Environment is created with the permissive baseline if
/// missing, and `project` is added to its eligibility by exact name unless
/// it already admits every Project.
pub async fn seed_project_overlay(
    pg: &PostgresDb,
    org: &str,
    project: &str,
    name: &str,
    rules: EnvironmentProtectionRules,
) {
    let existing = pg.get_org_environment(org, name).await.unwrap();
    let env = match existing {
        Some(env) => env,
        None => pg
            .put_org_environment(&OrgEnvironment {
                org: org.into(),
                name: name.into(),
                display_name: None,
                description: String::new(),
                protection: Default::default(),
                eligibility: Default::default(),
                created_at: 0,
                updated_at: 0,
                updated_by: "test".into(),
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
                "test",
                0,
            )
            .await
            .unwrap();
        }
    }
    let overlay = EnvironmentProjectOverlay {
        org: org.into(),
        environment: name.into(),
        project: project.into(),
        rules,
    };
    pg.put_environment_project_overlay(&overlay, "test", 0)
        .await
        .unwrap();
}
