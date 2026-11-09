//! The conditional overlay and privilege-grant writes (ADR-0073 §3): one
//! contract, run against the in-memory governance store (`scarab_testkit`,
//! always) and the Postgres adapter (a throwaway database per test, skipping
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset).
//!
//! For every stored field, a concurrent write changes that field alone, and
//! a write that expected the old value must be refused with `Conflict`. A
//! field left out of the compare (a column missing from the Postgres
//! `WHERE`, a field the in-memory store does not compare) lets the stale
//! write through and fails here.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_project::{
    EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProjectPrivilegeGrant, EnvironmentProtectionRules, ImageGrant, OrgEnvironment,
    OrgEnvironmentError, OrgEnvironmentStore,
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

#[track_caller]
fn assert_conflict(r: Result<(), OrgEnvironmentError>, what: &str) {
    assert!(
        matches!(r, Err(OrgEnvironmentError::Conflict(_))),
        "a write expecting the old {what} must be refused: {r:?}"
    );
}

fn overlay(project: &str, rules: EnvironmentProtectionRules) -> EnvironmentProjectOverlay {
    EnvironmentProjectOverlay {
        org: "acme".into(),
        environment: "prod".into(),
        project: project.into(),
        rules,
    }
}

async fn stored_overlay(
    s: &dyn EnvironmentGovernanceStore,
    project: &str,
) -> Option<EnvironmentProtectionRules> {
    s.deploy_governance("acme", project, "prod")
        .await
        .unwrap()
        .unwrap()
        .overlay
        .map(|o| o.rules)
}

async fn every_overlay_field_is_compared(s: &dyn EnvironmentGovernanceStore) {
    let base = EnvironmentProtectionRules {
        approvers: vec!["amy".into()],
        allowed_refs: vec!["refs/heads/main".into()],
        wait_timer_s: 30,
        require_reason: true,
        approval_gate: Default::default(),
    };
    // Every field is named, so a new one fails to compile here until it has
    // a case below. `approval_gate` has none: an overlay stores no approval
    // gate settings, so a write cannot move it.
    let EnvironmentProtectionRules {
        approvers: _,
        allowed_refs: _,
        wait_timer_s: _,
        require_reason: _,
        approval_gate: _,
    } = &base;
    let cases: Vec<(&str, EnvironmentProtectionRules)> = vec![
        (
            "approvers",
            EnvironmentProtectionRules {
                approvers: vec!["amy".into(), "bob".into()],
                ..base.clone()
            },
        ),
        (
            "allowed_refs",
            EnvironmentProtectionRules {
                allowed_refs: vec!["refs/heads/*".into()],
                ..base.clone()
            },
        ),
        (
            "wait_timer_s",
            EnvironmentProtectionRules {
                wait_timer_s: 31,
                ..base.clone()
            },
        ),
        (
            "require_reason",
            EnvironmentProtectionRules {
                require_reason: false,
                ..base.clone()
            },
        ),
    ];
    for (field, moved) in cases {
        // One Project per field, so the cases cannot see each other.
        let project = field.replace('_', "-");
        let old = overlay(&project, base.clone());
        let new = overlay(&project, moved);
        s.put_environment_project_overlay(&old, "amy", 1)
            .await
            .unwrap();
        // The concurrent write: this field alone changes.
        s.put_environment_project_overlay(&new, "bob", 2)
            .await
            .unwrap();
        assert_conflict(
            s.put_environment_project_overlay_expecting(&old, Some(Some(&old)), "amy", 3)
                .await,
            field,
        );
        assert_eq!(
            stored_overlay(s, &project).await,
            Some(new.rules.clone()),
            "{field}: a refused write changes nothing"
        );
        // Expecting what is stored succeeds, so the refusal above was the
        // compare on this field.
        s.put_environment_project_overlay_expecting(&old, Some(Some(&new)), "amy", 4)
            .await
            .unwrap();
        assert_eq!(
            stored_overlay(s, &project).await,
            Some(old.rules),
            "{field}"
        );
    }
}

fn grant(project: &str, images: Vec<ImageGrant>, permit: bool) -> EnvironmentProjectPrivilegeGrant {
    EnvironmentProjectPrivilegeGrant {
        org: "acme".into(),
        environment: "prod".into(),
        project: project.into(),
        privileged_images: images,
        permit_k8s_overlay: permit,
    }
}

async fn stored_grant(
    s: &dyn EnvironmentGovernanceStore,
    project: &str,
) -> Option<EnvironmentProjectPrivilegeGrant> {
    s.deploy_governance("acme", project, "prod")
        .await
        .unwrap()
        .unwrap()
        .privilege_grant
}

async fn every_grant_field_is_compared(s: &dyn EnvironmentGovernanceStore) {
    let image = ImageGrant {
        image_digest: format!("sha256:{}", "a".repeat(64)),
        privileged: false,
        capabilities: vec!["NET_ADMIN".into()],
    };
    let base = grant("base", vec![image.clone()], false);
    // Every field is named, as for overlays. The key fields name the row;
    // the whitelist is compared as one value, so each of an image's fields
    // has a case too.
    let EnvironmentProjectPrivilegeGrant {
        org: _,
        environment: _,
        project: _,
        privileged_images: _,
        permit_k8s_overlay: _,
    } = &base;
    let ImageGrant {
        image_digest: _,
        privileged: _,
        capabilities: _,
    } = &image;
    let cases: Vec<(&str, Vec<ImageGrant>, bool)> = vec![
        (
            "privileged_images.image_digest",
            vec![ImageGrant {
                image_digest: format!("sha256:{}", "b".repeat(64)),
                ..image.clone()
            }],
            false,
        ),
        (
            "privileged_images.privileged",
            vec![ImageGrant {
                privileged: true,
                ..image.clone()
            }],
            false,
        ),
        (
            "privileged_images.capabilities",
            vec![ImageGrant {
                capabilities: vec!["NET_ADMIN".into(), "SYS_TIME".into()],
                ..image.clone()
            }],
            false,
        ),
        (
            "privileged_images (one more image)",
            vec![
                image.clone(),
                ImageGrant {
                    image_digest: format!("sha256:{}", "c".repeat(64)),
                    ..image.clone()
                },
            ],
            false,
        ),
        ("permit_k8s_overlay", vec![image.clone()], true),
    ];
    for (i, (field, images, permit)) in cases.into_iter().enumerate() {
        let project = format!("web-{i}");
        let old = grant(
            &project,
            base.privileged_images.clone(),
            base.permit_k8s_overlay,
        );
        let new = grant(&project, images, permit);
        s.put_environment_project_privilege_grant(&old, "amy", 1)
            .await
            .unwrap();
        s.put_environment_project_privilege_grant(&new, "bob", 2)
            .await
            .unwrap();
        assert_conflict(
            s.put_environment_project_privilege_grant_expecting(&old, Some(Some(&old)), "amy", 3)
                .await,
            field,
        );
        assert_eq!(
            stored_grant(s, &project).await,
            Some(new.clone()),
            "{field}: a refused write changes nothing"
        );
        s.put_environment_project_privilege_grant_expecting(&old, Some(Some(&new)), "amy", 4)
            .await
            .unwrap();
        assert_eq!(stored_grant(s, &project).await, Some(old), "{field}");
    }
}

/// A new overlay (`Some(None)`) is only for a Project the Environment
/// admits, checked in the write itself: the API's earlier read could be
/// overtaken by an eligibility withdrawal.
async fn overlay_insert_requires_eligibility(s: &dyn EnvironmentGovernanceStore) {
    s.put_environment_eligibility_expecting(
        "acme",
        "prod",
        &EnvironmentProjectEligibility::Projects(vec!["web".into()]),
        None,
        "amy",
        1,
    )
    .await
    .unwrap();
    let rules = EnvironmentProtectionRules {
        require_reason: true,
        ..Default::default()
    };
    let refused = s
        .put_environment_project_overlay_expecting(
            &overlay("api", rules.clone()),
            Some(None),
            "amy",
            2,
        )
        .await;
    assert!(
        matches!(&refused, Err(OrgEnvironmentError::Conflict(m)) if m.contains("not eligible")),
        "a new overlay for a Project the Environment does not admit: {refused:?}"
    );
    assert_eq!(stored_overlay(s, "api").await, None, "nothing was stored");
    s.put_environment_project_overlay_expecting(
        &overlay("web", rules.clone()),
        Some(None),
        "amy",
        3,
    )
    .await
    .unwrap();
    assert_eq!(stored_overlay(s, "web").await, Some(rules));
}

macro_rules! contract_test {
    ($name:ident, $contract:ident) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn in_memory() {
                let s = InMemoryEnvironmentGovernance::new().with_environment(
                    "acme",
                    "prod",
                    EnvironmentProjectEligibility::AllProjects,
                );
                $contract(&s).await;
            }

            #[tokio::test]
            async fn postgres() {
                let Some(tdb) = fresh_db().await else {
                    eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
                    return;
                };
                let db = PostgresDb::with_pool(tdb.pool.clone());
                db.migrate().await.unwrap();
                db.put_org_environment(&environment("prod")).await.unwrap();
                db.put_environment_eligibility(
                    "acme",
                    "prod",
                    &EnvironmentProjectEligibility::AllProjects,
                    "seed",
                    1,
                )
                .await
                .unwrap();
                $contract(&db).await;
                tdb.cleanup().await;
            }
        }
    };
}

contract_test!(overlay_expecting, every_overlay_field_is_compared);
contract_test!(grant_expecting, every_grant_field_is_compared);
contract_test!(overlay_insert, overlay_insert_requires_eligibility);
