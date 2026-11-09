//! Deleting an org Environment with what hangs off it (git-bug a144fb1):
//! one contract, run against the in-memory governance store
//! (`scarab_testkit`, always) and the Postgres adapter (a throwaway database
//! per test, skipping cleanly when `SCARAB_TEST_DATABASE_URL` is unset).
//! The cascade deletes the Environment, its eligibility and every Project's
//! overlay and privilege grant in it, and nothing of another Environment.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_project::{
    EnvironmentCascadeDeletion, EnvironmentGovernanceStore, EnvironmentProjectEligibility,
    EnvironmentProjectOverlay, EnvironmentProjectPrivilegeGrant, EnvironmentProtectionRules,
    ImageGrant, OrgEnvironment, OrgEnvironmentError, OrgEnvironmentStore,
};
use scarab_testkit::InMemoryEnvironmentGovernance;

fn environment(name: &str) -> OrgEnvironment {
    OrgEnvironment {
        org: "acme".into(),
        name: name.into(),
        display_name: None,
        description: String::new(),
        protection: Default::default(),
        eligibility: Default::default(),
        created_at: 1,
        updated_at: 1,
        updated_by: "seed".into(),
    }
}

fn overlay(environment: &str, project: &str) -> EnvironmentProjectOverlay {
    EnvironmentProjectOverlay {
        org: "acme".into(),
        environment: environment.into(),
        project: project.into(),
        rules: EnvironmentProtectionRules {
            approvers: vec!["carol".into()],
            ..Default::default()
        },
    }
}

fn grant(environment: &str, project: &str) -> EnvironmentProjectPrivilegeGrant {
    EnvironmentProjectPrivilegeGrant {
        org: "acme".into(),
        environment: environment.into(),
        project: project.into(),
        privileged_images: vec![ImageGrant {
            image_digest: format!("sha256:{}", "a".repeat(64)),
            privileged: true,
            capabilities: vec![],
        }],
        permit_k8s_overlay: false,
    }
}

/// `prod` and `staging` both admit `web*` and `api`; `prod` holds an
/// overlay for `web` and grants for `web` and `api`, `staging` an overlay
/// for `web`.
async fn seed(s: &dyn EnvironmentGovernanceStore) {
    for env in ["prod", "staging"] {
        s.put_environment_eligibility(
            "acme",
            env,
            &EnvironmentProjectEligibility::Projects(vec!["api".into(), "web*".into()]),
            "seed",
            2,
        )
        .await
        .unwrap();
        s.put_environment_project_overlay(&overlay(env, "web"), "seed", 3)
            .await
            .unwrap();
    }
    for project in ["web", "api"] {
        s.put_environment_project_privilege_grant(&grant("prod", project), "seed", 4)
            .await
            .unwrap();
    }
}

async fn cascade_deletes_the_environment_and_its_projects(s: &dyn EnvironmentGovernanceStore) {
    seed(s).await;
    let staging_before = s.environment_projects("acme", "staging").await.unwrap();

    assert_eq!(
        s.delete_environment_cascade("acme", "prod").await.unwrap(),
        Some(EnvironmentCascadeDeletion {
            overlays: vec!["web".into()],
            privilege_grants: vec!["api".into(), "web".into()],
        })
    );
    assert_eq!(s.org_environment("acme", "prod").await.unwrap(), None);
    assert!(
        matches!(
            s.environment_projects("acme", "prod").await,
            Err(OrgEnvironmentError::NotFound { .. })
        ),
        "the Environment is gone"
    );
    assert_eq!(
        s.environment_projects("acme", "staging").await.unwrap(),
        staging_before,
        "another Environment's overlay is not touched"
    );
    // A second delete finds nothing and changes nothing.
    assert_eq!(
        s.delete_environment_cascade("acme", "prod").await.unwrap(),
        None
    );
    assert!(s
        .org_environment("acme", "staging")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn cascade_in_memory() {
    let s = InMemoryEnvironmentGovernance::new()
        .with_environment("acme", "prod", EnvironmentProjectEligibility::default())
        .with_environment("acme", "staging", EnvironmentProjectEligibility::default());
    cascade_deletes_the_environment_and_its_projects(&s).await;
}

/// On Postgres, also: no row of the old Environment is left behind, so one
/// created again under the name starts with no overlay, grant or eligible
/// Project; and a plain delete is still refused while overlays exist.
#[tokio::test]
async fn cascade_postgres() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    for env in ["prod", "staging"] {
        db.put_org_environment(&environment(env)).await.unwrap();
    }
    cascade_deletes_the_environment_and_its_projects(&db).await;

    db.put_org_environment(&environment("prod")).await.unwrap();
    assert_eq!(
        db.environment_projects("acme", "prod").await.unwrap(),
        vec![]
    );
    assert_eq!(
        db.org_environment("acme", "prod")
            .await
            .unwrap()
            .unwrap()
            .eligibility,
        EnvironmentProjectEligibility::default()
    );
    assert!(matches!(
        db.delete_org_environment("acme", "staging").await,
        Err(OrgEnvironmentError::InUse(_))
    ));
    tdb.cleanup().await;
}
