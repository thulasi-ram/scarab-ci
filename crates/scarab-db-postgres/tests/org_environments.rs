//! The minimal org-level Environment (ADR-0071, migrations 0054 and 0057)
//! against *real* Postgres.
//!
//! The rules on one OrgEnvironment are pure and tested in `scarab-project`;
//! what only Postgres can show is what spans rows: a replace keeps
//! `created_at`, two Orgs never see each other's names, and every
//! loosenable governance write has a conditional form. Every test gets its
//! own database (`fresh_db`). Skips cleanly when `SCARAB_TEST_DATABASE_URL`
//! is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_project::{OrgEnvironment, OrgEnvironmentError, OrgEnvironmentStore};

fn org_environment(org: &str, name: &str, at: i64) -> OrgEnvironment {
    OrgEnvironment {
        org: org.into(),
        name: name.into(),
        description: format!("{name} for {org}"),
        // The governance half has its own writes (ADR-0071); a new
        // Environment stores the permissive, fail-closed defaults.
        protection: Default::default(),
        eligibility: Default::default(),
        created_at: at,
        updated_at: at,
        updated_by: "amy".into(),
    }
}

async fn migrated() -> Option<(common::TestDb, PostgresDb)> {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return None;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    Some((tdb, db))
}

#[tokio::test]
async fn put_get_list_round_trip() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let prod = org_environment("acme", "prod", 1_000);
    let staging = org_environment("acme", "staging", 1_100);

    let stored = db.put_org_environment(&prod).await.unwrap();
    assert_eq!(stored, prod, "put returns exactly what is stored");
    db.put_org_environment(&staging).await.unwrap();

    assert_eq!(
        db.get_org_environment("acme", "prod").await.unwrap(),
        Some(prod.clone())
    );
    assert_eq!(db.get_org_environment("acme", "dev").await.unwrap(), None);
    // Names are exact and case-sensitive: `Prod` is not `prod`.
    assert_eq!(db.get_org_environment("acme", "Prod").await.unwrap(), None);
    assert_eq!(
        db.list_org_environments("acme").await.unwrap(),
        vec![prod, staging],
        "by name"
    );
    assert_eq!(db.list_org_environments("initech").await.unwrap(), vec![]);

    tdb.cleanup().await;
}

#[tokio::test]
async fn a_replace_swaps_the_description_and_keeps_created_at() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.put_org_environment(&org_environment("acme", "prod", 1_000))
        .await
        .unwrap();
    let mut replacement = org_environment("acme", "prod", 2_000);
    replacement.description = "the real prod".into();
    replacement.updated_by = "bob".into();
    let stored = db.put_org_environment(&replacement).await.unwrap();

    assert_eq!(stored.created_at, 1_000, "a replace keeps created_at");
    assert_eq!(stored.updated_at, 2_000);
    assert_eq!(stored.updated_by, "bob");
    assert_eq!(stored.description, "the real prod");
    assert_eq!(
        db.get_org_environment("acme", "prod").await.unwrap(),
        Some(stored)
    );

    tdb.cleanup().await;
}

#[tokio::test]
async fn an_invalid_environment_is_refused_before_anything_is_written() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let dots = org_environment("acme", "..", 1_000);
    assert!(matches!(
        db.put_org_environment(&dots).await,
        Err(OrgEnvironmentError::Invalid(_))
    ));
    assert_eq!(db.list_org_environments("acme").await.unwrap(), vec![]);

    tdb.cleanup().await;
}

#[tokio::test]
async fn two_orgs_with_the_same_name_are_independent() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.put_org_environment(&org_environment("acme", "prod", 1_000))
        .await
        .unwrap();
    // Same name, another Org: no collision.
    let globex = db
        .put_org_environment(&org_environment("globex", "prod", 1_100))
        .await
        .unwrap();
    assert!(db.delete_org_environment("acme", "prod").await.unwrap());
    assert_eq!(
        db.get_org_environment("globex", "prod").await.unwrap(),
        Some(globex),
        "deleting acme's prod leaves globex's alone"
    );
    assert_eq!(db.list_org_environments("acme").await.unwrap(), vec![]);
    tdb.cleanup().await;
}

/// ADR-0073 §3: every loosenable governance write has an `…_expecting` form
/// that writes only if what is stored is still what the caller judged the
/// write against — otherwise `Conflict`, and nothing changes. `None` stays
/// the unconditional write.
#[tokio::test]
async fn an_expecting_write_refuses_when_the_stored_value_moved() {
    use scarab_project::{
        EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
        EnvironmentProjectPrivilegeGrant, EnvironmentProtectionRules, ImageGrant,
    };

    let Some((tdb, db)) = migrated().await else {
        return;
    };
    fn conflict<T: std::fmt::Debug>(r: Result<T, OrgEnvironmentError>) {
        assert!(
            matches!(r, Err(OrgEnvironmentError::Conflict(_))),
            "expected a conflict, got {r:?}"
        )
    }

    db.put_org_environment(&org_environment("acme", "prod", 1))
        .await
        .unwrap();

    // Baseline protection.
    let strict = EnvironmentProtectionRules {
        approvers: vec!["amy".into()],
        allowed_refs: vec!["refs/heads/main".into()],
        wait_timer_s: 5,
        require_reason: true,
    };
    let permissive = EnvironmentProtectionRules::default();
    db.put_environment_protection_expecting("acme", "prod", &strict, Some(&permissive), "amy", 4)
        .await
        .unwrap();
    conflict(
        db.put_environment_protection_expecting(
            "acme",
            "prod",
            &permissive,
            Some(&permissive),
            "amy",
            5,
        )
        .await,
    );
    assert_eq!(
        db.get_org_environment("acme", "prod")
            .await
            .unwrap()
            .unwrap()
            .protection,
        strict,
        "nothing changed"
    );
    assert!(matches!(
        db.put_environment_protection_expecting("acme", "nope", &strict, Some(&strict), "amy", 5)
            .await,
        Err(OrgEnvironmentError::NotFound { .. })
    ));

    // Eligibility, compared as a set.
    let two = EnvironmentProjectEligibility::Projects(vec!["web".into(), "api".into()]);
    db.put_environment_eligibility_expecting(
        "acme",
        "prod",
        &two,
        Some(&EnvironmentProjectEligibility::Projects(vec![])),
        "amy",
        6,
    )
    .await
    .unwrap();
    conflict(
        db.put_environment_eligibility_expecting(
            "acme",
            "prod",
            &EnvironmentProjectEligibility::AllProjects,
            Some(&EnvironmentProjectEligibility::Projects(vec!["web".into()])),
            "amy",
            7,
        )
        .await,
    );
    db.put_environment_eligibility_expecting(
        "acme",
        "prod",
        &EnvironmentProjectEligibility::Projects(vec!["web".into()]),
        Some(&EnvironmentProjectEligibility::Projects(vec![
            "api".into(),
            "web".into(),
        ])),
        "amy",
        8,
    )
    .await
    .unwrap();

    // An overlay: expected-absent creates; a second expected-absent, or a
    // stale expected value, is refused.
    let overlay = EnvironmentProjectOverlay {
        org: "acme".into(),
        environment: "prod".into(),
        project: "web".into(),
        rules: strict.clone(),
    };
    db.put_environment_project_overlay_expecting(&overlay, Some(None), "amy", 9)
        .await
        .unwrap();
    conflict(
        db.put_environment_project_overlay_expecting(&overlay, Some(None), "amy", 10)
            .await,
    );
    let looser = EnvironmentProjectOverlay {
        rules: permissive.clone(),
        ..overlay.clone()
    };
    conflict(
        db.put_environment_project_overlay_expecting(&looser, Some(Some(&looser)), "amy", 11)
            .await,
    );
    db.put_environment_project_overlay_expecting(&looser, Some(Some(&overlay)), "amy", 12)
        .await
        .unwrap();
    let g = db
        .deploy_governance("acme", "web", "prod")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(g.overlay.unwrap().rules, permissive);

    // A privilege grant, the same way.
    let grant = EnvironmentProjectPrivilegeGrant {
        org: "acme".into(),
        environment: "prod".into(),
        project: "web".into(),
        privileged_images: vec![ImageGrant {
            image_digest: "sha256:aa".into(),
            privileged: false,
            capabilities: vec!["NET_ADMIN".into()],
        }],
        permit_k8s_overlay: false,
    };
    db.put_environment_project_privilege_grant_expecting(&grant, Some(None), "amy", 13)
        .await
        .unwrap();
    conflict(
        db.put_environment_project_privilege_grant_expecting(&grant, Some(None), "amy", 14)
            .await,
    );
    let wider = EnvironmentProjectPrivilegeGrant {
        permit_k8s_overlay: true,
        ..grant.clone()
    };
    conflict(
        db.put_environment_project_privilege_grant_expecting(&wider, Some(Some(&wider)), "amy", 15)
            .await,
    );
    db.put_environment_project_privilege_grant_expecting(&wider, Some(Some(&grant)), "amy", 16)
        .await
        .unwrap();
    assert!(matches!(
        db.put_environment_project_privilege_grant_expecting(
            &EnvironmentProjectPrivilegeGrant {
                environment: "nope".into(),
                ..wider.clone()
            },
            Some(Some(&wider)),
            "amy",
            17,
        )
        .await,
        Err(OrgEnvironmentError::NotFound { .. })
    ));

    // `None` is the unconditional write it always was.
    db.put_environment_protection_expecting("acme", "prod", &permissive, None, "amy", 18)
        .await
        .unwrap();

    tdb.cleanup().await;
}
