//! Duplicating an org Environment (git-bug d002a0c): one contract, run
//! against the in-memory governance store (`scarab_testkit`, always) and the
//! Postgres adapter (a throwaway database per test, skipping cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset). The copy carries the baseline, the
//! eligibility and every Project's overlay and grant; the source is not
//! changed; a taken name changes nothing.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_project::{
    ApprovalGateRules, EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProjectPrivilegeGrant, EnvironmentProtectionRules, ImageGrant, OrgEnvironment,
    MissingGateMode, OrgEnvironmentError, OrgEnvironmentStore,
};
use scarab_testkit::InMemoryEnvironmentGovernance;

fn environment(name: &str) -> OrgEnvironment {
    OrgEnvironment {
        org: "acme".into(),
        name: name.into(),
        display_name: Some("Production".into()),
        description: "the one prod".into(),
        protection: Default::default(),
        eligibility: Default::default(),
        created_at: 1,
        updated_at: 1,
        updated_by: "seed".into(),
    }
}

/// `prod` with a strict baseline, two eligible patterns, an overlay for
/// `web` and grants for `web` and `api`.
async fn seed(s: &dyn EnvironmentGovernanceStore) {
    s.put_environment_protection(
        "acme",
        "prod",
        &EnvironmentProtectionRules {
            approvers: vec!["amy".into(), "bob".into()],
            allowed_refs: vec!["refs/heads/main".into()],
            wait_timer_s: 30,
            require_reason: true,
            approval_gate: ApprovalGateRules {
                missing_gate: MissingGateMode::Refuse,
                added_gate_expiry_s: Some(3600),
                required_approvals: Some(1),
                max_approval_age_s: 600,
            },
        },
        "seed",
        2,
    )
    .await
    .unwrap();
    s.put_environment_eligibility(
        "acme",
        "prod",
        &EnvironmentProjectEligibility::Projects(vec!["api".into(), "web*".into()]),
        "seed",
        3,
    )
    .await
    .unwrap();
    s.put_environment_project_overlay(
        &EnvironmentProjectOverlay {
            org: "acme".into(),
            environment: "prod".into(),
            project: "web".into(),
            rules: EnvironmentProtectionRules {
                approvers: vec!["carol".into()],
                allowed_refs: vec!["refs/tags/*".into()],
                wait_timer_s: 60,
                require_reason: false,
                approval_gate: ApprovalGateRules::default(),
            },
        },
        "seed",
        4,
    )
    .await
    .unwrap();
    for (project, permit) in [("web", true), ("api", false)] {
        s.put_environment_project_privilege_grant(
            &EnvironmentProjectPrivilegeGrant {
                org: "acme".into(),
                environment: "prod".into(),
                project: project.into(),
                privileged_images: vec![ImageGrant {
                    image_digest: format!("sha256:{}", "a".repeat(64)),
                    privileged: true,
                    capabilities: vec!["NET_ADMIN".into()],
                }],
                permit_k8s_overlay: permit,
            },
            "seed",
            5,
        )
        .await
        .unwrap();
    }
}

async fn duplicate_copies_everything_and_leaves_the_source(s: &dyn EnvironmentGovernanceStore) {
    seed(s).await;
    let before = s.org_environment("acme", "prod").await.unwrap().unwrap();
    let projects_before = s.environment_projects("acme", "prod").await.unwrap();

    let copy = s
        .duplicate_environment(
            "acme",
            "prod",
            "prod-eu",
            Some("Production (EU)"),
            "amy",
            99,
        )
        .await
        .unwrap();
    assert_eq!(
        copy,
        OrgEnvironment {
            name: "prod-eu".into(),
            display_name: Some("Production (EU)".into()),
            created_at: 99,
            updated_at: 99,
            updated_by: "amy".into(),
            ..before.clone()
        },
        "the baseline, eligibility and description are the source's"
    );
    assert_eq!(
        copy.protection.approval_gate,
        ApprovalGateRules {
            missing_gate: MissingGateMode::Refuse,
            added_gate_expiry_s: Some(3600),
            required_approvals: Some(1),
            max_approval_age_s: 600,
        },
        "the approval gate settings are copied"
    );
    assert_eq!(
        s.org_environment("acme", "prod-eu").await.unwrap(),
        Some(copy)
    );

    // Every Project's overlay and grant, re-keyed to the copy.
    let copied = s.environment_projects("acme", "prod-eu").await.unwrap();
    assert_eq!(copied.len(), 2, "{copied:?}");
    for (c, b) in copied.iter().zip(&projects_before) {
        assert_eq!(c.project, b.project);
        assert_eq!(c.eligible, b.eligible);
        assert_eq!(
            c.overlay
                .as_ref()
                .map(|o| (&o.rules, o.environment.as_str())),
            b.overlay.as_ref().map(|o| (&o.rules, "prod-eu"))
        );
        assert_eq!(
            c.privilege_grant.as_ref().map(|g| (
                &g.privileged_images,
                g.permit_k8s_overlay,
                g.environment.as_str()
            )),
            b.privilege_grant.as_ref().map(|g| (
                &g.privileged_images,
                g.permit_k8s_overlay,
                "prod-eu"
            ))
        );
    }

    // The source is unchanged.
    assert_eq!(
        s.org_environment("acme", "prod").await.unwrap(),
        Some(before)
    );
    assert_eq!(
        s.environment_projects("acme", "prod").await.unwrap(),
        projects_before
    );
}

async fn a_taken_name_or_a_missing_source_changes_nothing(s: &dyn EnvironmentGovernanceStore) {
    seed(s).await;
    s.duplicate_environment("acme", "prod", "staging", None, "amy", 10)
        .await
        .unwrap();
    // Make the existing target differ from the source, so a copy over it
    // would show.
    s.put_environment_eligibility(
        "acme",
        "staging",
        &EnvironmentProjectEligibility::AllProjects,
        "amy",
        11,
    )
    .await
    .unwrap();
    s.delete_environment_project_overlay("acme", "staging", "web")
        .await
        .unwrap();
    let staging = s.org_environment("acme", "staging").await.unwrap();
    let staging_projects = s.environment_projects("acme", "staging").await.unwrap();

    let taken = s
        .duplicate_environment("acme", "prod", "staging", Some("Other"), "amy", 12)
        .await;
    assert!(
        matches!(taken, Err(OrgEnvironmentError::Conflict(_))),
        "{taken:?}"
    );
    assert_eq!(s.org_environment("acme", "staging").await.unwrap(), staging);
    assert_eq!(
        s.environment_projects("acme", "staging").await.unwrap(),
        staging_projects
    );

    let missing = s
        .duplicate_environment("acme", "nope", "fresh", None, "amy", 13)
        .await;
    assert!(
        matches!(missing, Err(OrgEnvironmentError::NotFound { .. })),
        "{missing:?}"
    );
    assert_eq!(s.org_environment("acme", "fresh").await.unwrap(), None);

    let invalid = s
        .duplicate_environment("acme", "prod", "..", None, "amy", 14)
        .await;
    assert!(
        matches!(invalid, Err(OrgEnvironmentError::Invalid(_))),
        "{invalid:?}"
    );
}

/// A claim on a name excludes a second claim on that name in that org, and
/// no other; dropping it releases it. Postgres releases it when the
/// connection goes back to the pool, a moment after the drop, so the last
/// check waits for it.
async fn a_claim_excludes_a_second_until_dropped(s: &dyn EnvironmentGovernanceStore) {
    let first = s
        .claim_environment_name("acme", "prod-eu")
        .await
        .unwrap()
        .expect("the first claim");
    assert!(s
        .claim_environment_name("acme", "prod-eu")
        .await
        .unwrap()
        .is_none());
    let other_name = s.claim_environment_name("acme", "prod-us").await.unwrap();
    let other_org = s.claim_environment_name("beta", "prod-eu").await.unwrap();
    assert!(other_name.is_some() && other_org.is_some());

    drop(first);
    let mut again = None;
    for _ in 0..100 {
        again = s.claim_environment_name("acme", "prod-eu").await.unwrap();
        if again.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(again.is_some(), "the claim is released once dropped");
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
                    EnvironmentProjectEligibility::Projects(Vec::new()),
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
                $contract(&db).await;
                tdb.cleanup().await;
            }
        }
    };
}

contract_test!(
    duplicate_copies_everything,
    duplicate_copies_everything_and_leaves_the_source
);
contract_test!(
    duplicate_onto_a_taken_name_changes_nothing,
    a_taken_name_or_a_missing_source_changes_nothing
);
contract_test!(
    a_name_claim_is_exclusive,
    a_claim_excludes_a_second_until_dropped
);

/// The Postgres duplicate copies every column of these tables, a column a
/// later migration adds included; the in-memory store copies the struct's
/// fields. A new column fails this test until someone decides whether a
/// Duplicate carries it (an acknowledgement or a version should not), seeds
/// it in `seed` above so the contract shows both stores agree, and adds it
/// here.
#[tokio::test]
async fn duplicate_knows_every_copied_column() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let known: &[(&str, &[&str])] = &[
        (
            "org_environments",
            &[
                "added_gate_expiry_s",
                "allowed_refs",
                "approvers",
                "created_at",
                "description",
                "display_name",
                "eligible_all_projects",
                "max_approval_age_s",
                "missing_gate_mode",
                "name",
                "org",
                "require_reason",
                "required_approvals",
                "updated_at",
                "updated_by",
                "wait_timer_s",
            ],
        ),
        (
            "environment_project_eligibility",
            &["environment", "org", "project_pattern"],
        ),
        (
            "environment_project_overlays",
            &[
                "allowed_refs",
                "approvers",
                "environment",
                "org",
                "project",
                "require_reason",
                "updated_at",
                "updated_by",
                "wait_timer_s",
            ],
        ),
        (
            "environment_project_privilege_grants",
            &[
                "environment",
                "org",
                "permit_k8s_overlay",
                "privileged_images",
                "project",
                "updated_at",
                "updated_by",
            ],
        ),
    ];
    let mut stored_all = Vec::new();
    let mut known_all = Vec::new();
    for (table, columns) in known {
        let mut stored: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns
             WHERE table_schema = current_schema() AND table_name = $1",
        )
        .bind(table)
        .fetch_all(&tdb.pool)
        .await
        .unwrap();
        stored.sort();
        let mut columns: Vec<String> = columns.iter().map(|c| c.to_string()).collect();
        columns.sort();
        stored_all.push((table.to_string(), stored));
        known_all.push((table.to_string(), columns));
    }
    tdb.cleanup().await;
    assert_eq!(stored_all, known_all);
}

/// The display name round-trips and a replace without one clears it (the
/// whole-Environment PUT is a full replace).
#[tokio::test]
async fn display_name_round_trips_on_postgres() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let stored = db.put_org_environment(&environment("prod")).await.unwrap();
    assert_eq!(stored.display_name.as_deref(), Some("Production"));
    assert_eq!(
        db.list_org_environments("acme").await.unwrap()[0]
            .display_name
            .as_deref(),
        Some("Production")
    );
    let cleared = db
        .put_org_environment(&OrgEnvironment {
            display_name: None,
            ..environment("prod")
        })
        .await
        .unwrap();
    assert_eq!(cleared.display_name, None);
    assert!(matches!(
        db.put_org_environment(&OrgEnvironment {
            display_name: Some("bad\u{7}".into()),
            ..environment("prod")
        })
        .await,
        Err(OrgEnvironmentError::Invalid(_))
    ));
    tdb.cleanup().await;
}
