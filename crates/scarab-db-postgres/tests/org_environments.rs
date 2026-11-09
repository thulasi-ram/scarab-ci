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
use scarab_project::{OrgEnvironment, OrgEnvironmentError, OrgEnvironmentStore, WritePrecondition};

fn org_environment(org: &str, name: &str, at: i64) -> OrgEnvironment {
    OrgEnvironment {
        org: org.into(),
        name: name.into(),
        display_name: None,
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
        ApprovalGateRules, EnvironmentGovernanceStore, EnvironmentProjectEligibility,
        EnvironmentProjectOverlay, EnvironmentProjectPrivilegeGrant, EnvironmentProtectionRules,
        ImageGrant, MissingGateMode,
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
    // Every approval gate setting off its default, so each column is
    // written and read back (migration 0079).
    let strict = EnvironmentProtectionRules {
        approvers: vec!["amy".into(), "bob".into()],
        allowed_refs: vec!["refs/heads/main".into()],
        wait_timer_s: 5,
        require_reason: true,
        approval_gate: ApprovalGateRules {
            missing_gate: MissingGateMode::Refuse,
            added_gate_expiry_s: Some(3600),
            required_approvals: Some(1),
            max_approval_age_s: 600,
        },
    };
    let permissive = EnvironmentProtectionRules::default();
    db.put_environment_protection_expecting("acme", "prod", &strict, Some(&permissive), "amy", 4)
        .await
        .unwrap();
    // An expectation that differs from what is stored in an approval gate
    // setting alone is a conflict too: a step-up judged against it would
    // be judged against the wrong rules.
    let mut stale = strict.clone();
    stale.approval_gate.max_approval_age_s = 60;
    conflict(
        db.put_environment_protection_expecting(
            "acme",
            "prod",
            &permissive,
            Some(&stale),
            "amy",
            5,
        )
        .await,
    );
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

/// git-bug 66cce00: a write that loses to a concurrent one at the database
/// (a serialization failure `40001`, or a deadlock `40P01`) is a retryable
/// [`OrgEnvironmentError::Conflict`], not a store failure (a 500). The
/// interleaving is constructed, not raced: tx1 holds the row, the store's
/// write (on a pool whose transactions run SERIALIZABLE) blocks on it,
/// then tx1 commits under it.
#[tokio::test]
async fn a_write_that_loses_a_serialization_race_is_a_conflict() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.put_org_environment(&org_environment("acme", "prod", 1_000))
        .await
        .unwrap();
    let rr_options = tdb
        .pool
        .connect_options()
        .as_ref()
        .clone()
        .options([("default_transaction_isolation", "serializable")]);
    let rr_pool = sqlx::PgPool::connect_with(rr_options).await.unwrap();
    let rr = std::sync::Arc::new(PostgresDb::with_pool(rr_pool.clone()));

    let mut tx1 = tdb.pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE org_environments SET description = 'tx1' WHERE org = 'acme' AND name = 'prod'",
    )
    .execute(&mut *tx1)
    .await
    .unwrap();
    let write = tokio::spawn({
        let rr = rr.clone();
        async move {
            rr.put_org_environment(&org_environment("acme", "prod", 2_000))
                .await
        }
    });
    // Wait until the store's write is blocked on tx1's row lock.
    let mut blocked = false;
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE wait_event_type = 'Lock' AND query LIKE '%INSERT INTO org_environments%'",
        )
        .fetch_one(&tdb.pool)
        .await
        .unwrap();
        if n > 0 {
            blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(blocked, "the store's write never blocked on tx1's row lock");
    tx1.commit().await.unwrap();

    let lost = write.await.unwrap();
    assert!(
        matches!(lost, Err(OrgEnvironmentError::Conflict(_))),
        "{lost:?}"
    );
    assert_eq!(
        db.get_org_environment("acme", "prod")
            .await
            .unwrap()
            .unwrap()
            .description,
        "tx1",
        "nothing of the losing write is stored"
    );
    rr_pool.close().await;
    tdb.cleanup().await;
}

/// A new overlay checks eligibility in its own write, under a share lock on
/// the Environment's row, so an eligibility withdrawal cannot commit between
/// the check and the insert. The interleaving is constructed: tx1 withdraws
/// `web` and holds the row, the store's insert blocks on it, then tx1 commits
/// under it and the insert is refused.
#[tokio::test]
async fn an_overlay_insert_waits_for_an_eligibility_withdrawal_and_is_refused() {
    use scarab_project::{
        EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    };
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.put_org_environment(&org_environment("acme", "prod", 1_000))
        .await
        .unwrap();
    db.put_environment_eligibility(
        "acme",
        "prod",
        &EnvironmentProjectEligibility::Projects(vec!["web".into()]),
        "amy",
        1_000,
    )
    .await
    .unwrap();

    // tx1 withdraws `web` as an eligibility write does: the row first.
    let mut tx1 = tdb.pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE org_environments SET eligible_all_projects = false, updated_at = 2000
         WHERE org = 'acme' AND name = 'prod'",
    )
    .execute(&mut *tx1)
    .await
    .unwrap();
    sqlx::query(
        "DELETE FROM environment_project_eligibility WHERE org = 'acme' AND environment = 'prod'",
    )
    .execute(&mut *tx1)
    .await
    .unwrap();
    let db = std::sync::Arc::new(db);
    let insert = tokio::spawn({
        let db = db.clone();
        async move {
            let overlay = EnvironmentProjectOverlay {
                org: "acme".into(),
                environment: "prod".into(),
                project: "web".into(),
                rules: Default::default(),
            };
            db.put_environment_project_overlay_expecting(&overlay, Some(None), "bob", 2_000)
                .await
        }
    });
    // Wait until the insert's eligibility check is blocked on tx1's row lock.
    let mut blocked = false;
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'
               AND query LIKE '%FOR SHARE%'",
        )
        .fetch_one(&tdb.pool)
        .await
        .unwrap();
        if n > 0 {
            blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        blocked,
        "the overlay insert never waited for tx1's row lock"
    );
    tx1.commit().await.unwrap();

    let refused = insert.await.unwrap();
    assert!(
        matches!(&refused, Err(OrgEnvironmentError::Conflict(m)) if m.contains("not eligible")),
        "{refused:?}"
    );
    assert_eq!(
        db.deploy_governance("acme", "web", "prod")
            .await
            .unwrap()
            .unwrap()
            .overlay,
        None,
        "no overlay for a Project the Environment no longer admits"
    );
    tdb.cleanup().await;
}

/// git-bug 66cce00: `If-None-Match: *` creates only; `If-Match` replaces only
/// the Environment whose tag the caller read, checked under the row lock.
#[tokio::test]
async fn preconditions_create_only_and_replace_only_what_was_read() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let prod = org_environment("acme", "prod", 1_000);
    let created = db
        .put_org_environment_if(&prod, &WritePrecondition::Absent)
        .await
        .unwrap();
    assert!(matches!(
        db.put_org_environment_if(&prod, &WritePrecondition::Absent)
            .await,
        Err(OrgEnvironmentError::PreconditionFailed(_))
    ));
    let read = created.etag();
    let edited = OrgEnvironment {
        description: "edited".into(),
        updated_at: 2_000,
        ..prod.clone()
    };
    let replaced = db
        .put_org_environment_if(&edited, &WritePrecondition::Matches(vec![read.clone()]))
        .await
        .unwrap();
    assert_ne!(replaced.etag(), read);
    // A second writer still holding the first tag loses, and stores nothing.
    let stale = OrgEnvironment {
        description: "stale".into(),
        updated_at: 3_000,
        ..prod.clone()
    };
    assert!(matches!(
        db.put_org_environment_if(&stale, &WritePrecondition::Matches(vec![read]))
            .await,
        Err(OrgEnvironmentError::PreconditionFailed(_))
    ));
    assert_eq!(
        db.get_org_environment("acme", "prod")
            .await
            .unwrap()
            .unwrap()
            .description,
        "edited"
    );
    // A tag for an Environment that is gone fails too.
    assert!(matches!(
        db.put_org_environment_if(
            &org_environment("acme", "dev", 1_000),
            &WritePrecondition::Matches(vec![replaced.etag()])
        )
        .await,
        Err(OrgEnvironmentError::PreconditionFailed(_))
    ));
    tdb.cleanup().await;
}
