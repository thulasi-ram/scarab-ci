//! The minimal org-level Environment (ADR-0071, migration 0054) against
//! *real* Postgres.
//!
//! The rules on one OrgEnvironment are pure and tested in `scarab-project`;
//! what only Postgres can show is what spans rows: a replace swaps the whole
//! source list atomically and keeps `created_at`, a source routes to exactly
//! one Environment per Org (a second claim is refused and changes nothing),
//! deleting an Environment takes its sources with it, and two Orgs never see
//! each other's names or sources. Every test gets its own database
//! (`fresh_db`). Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentDeploymentSource, OrgEnvironment,
    OrgEnvironmentError, OrgEnvironmentStore,
};

const MINUTE_MS: i64 = 60_000;

fn source(installation: &str, vendor_grouping: &str) -> EnvironmentDeploymentSource {
    EnvironmentDeploymentSource {
        installation: installation.into(),
        vendor_grouping: vendor_grouping.into(),
    }
}

fn org_environment(
    org: &str,
    name: &str,
    deadline_minutes: i64,
    sources: Vec<EnvironmentDeploymentSource>,
    at: i64,
) -> OrgEnvironment {
    OrgEnvironment {
        org: org.into(),
        name: name.into(),
        description: format!("{name} for {org}"),
        deployment_deadline: EnvironmentDeploymentDeadline::new(deadline_minutes * MINUTE_MS)
            .unwrap(),
        deployment_sources: sources,
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
async fn put_get_list_round_trip_with_sources_sorted() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let prod = org_environment(
        "acme",
        "prod",
        5,
        // Written out of order: the store keeps a set and returns it sorted.
        vec![
            source("argocd-prod", "prod"),
            source("argocd-eu", "prod-eu"),
        ],
        1_000,
    );
    let staging = org_environment("acme", "staging", 60, vec![], 1_100);

    let stored = db.put_org_environment(&prod).await.unwrap();
    let mut sorted = prod.clone();
    sorted.deployment_sources.sort();
    assert_eq!(stored, sorted, "put returns exactly what is stored");
    db.put_org_environment(&staging).await.unwrap();

    assert_eq!(
        db.get_org_environment("acme", "prod").await.unwrap(),
        Some(sorted.clone())
    );
    assert_eq!(db.get_org_environment("acme", "dev").await.unwrap(), None);
    // Names are exact and case-sensitive: `Prod` is not `prod`.
    assert_eq!(db.get_org_environment("acme", "Prod").await.unwrap(), None);
    assert_eq!(
        db.list_org_environments("acme").await.unwrap(),
        vec![sorted, staging],
        "by name"
    );
    assert_eq!(db.list_org_environments("initech").await.unwrap(), vec![]);

    tdb.cleanup().await;
}

#[tokio::test]
async fn a_replace_swaps_every_field_and_the_whole_source_list_and_keeps_created_at() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.put_org_environment(&org_environment(
        "acme",
        "prod",
        5,
        vec![source("argocd", "prod"), source("argocd", "prod-legacy")],
        1_000,
    ))
    .await
    .unwrap();

    let mut replacement = org_environment(
        "acme",
        "prod",
        30,
        vec![source("argocd", "prod"), source("flux", "clusters-prod")],
        2_000,
    );
    replacement.description = "the real prod".into();
    replacement.updated_by = "bob".into();
    let stored = db.put_org_environment(&replacement).await.unwrap();

    assert_eq!(stored.created_at, 1_000, "a replace keeps created_at");
    assert_eq!(stored.updated_at, 2_000);
    assert_eq!(stored.updated_by, "bob");
    assert_eq!(stored.description, "the real prod");
    assert_eq!(stored.deployment_deadline.millis(), 30 * MINUTE_MS);
    assert_eq!(
        stored.deployment_sources,
        vec![source("argocd", "prod"), source("flux", "clusters-prod")]
    );
    assert_eq!(
        db.get_org_environment("acme", "prod").await.unwrap(),
        Some(stored)
    );
    // The dropped source routes nowhere now, and is free to claim.
    assert_eq!(
        db.environment_for_deployment_source("acme", "argocd", "prod-legacy")
            .await
            .unwrap(),
        None
    );
    db.put_org_environment(&org_environment(
        "acme",
        "legacy",
        5,
        vec![source("argocd", "prod-legacy")],
        3_000,
    ))
    .await
    .unwrap();

    // An empty list clears every source.
    db.put_org_environment(&org_environment("acme", "prod", 30, vec![], 4_000))
        .await
        .unwrap();
    assert_eq!(
        db.environment_for_deployment_source("acme", "argocd", "prod")
            .await
            .unwrap(),
        None
    );

    tdb.cleanup().await;
}

#[tokio::test]
async fn a_source_another_environment_holds_is_refused_and_nothing_changes() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let prod = db
        .put_org_environment(&org_environment(
            "acme",
            "prod",
            5,
            vec![source("argocd", "prod")],
            1_000,
        ))
        .await
        .unwrap();
    let staging = db
        .put_org_environment(&org_environment(
            "acme",
            "staging",
            10,
            vec![source("argocd", "staging")],
            1_100,
        ))
        .await
        .unwrap();

    // staging tries to take prod's source alongside a change to its own row.
    let mut grab = org_environment(
        "acme",
        "staging",
        20,
        vec![source("argocd", "staging"), source("argocd", "prod")],
        2_000,
    );
    grab.description = "should not land".into();
    assert_eq!(
        db.put_org_environment(&grab).await,
        Err(OrgEnvironmentError::DeploymentSourceTaken {
            installation: "argocd".into(),
            vendor_grouping: "prod".into(),
            environment: "prod".into(),
        })
    );
    // Both rows exactly as they were — the replace rolled back whole,
    // including staging's own fields and its own (deleted-then-reinserted)
    // source.
    assert_eq!(
        db.get_org_environment("acme", "prod").await.unwrap(),
        Some(prod)
    );
    assert_eq!(
        db.get_org_environment("acme", "staging").await.unwrap(),
        Some(staging)
    );

    // A brand-new Environment claiming it is refused too, and not created.
    assert!(matches!(
        db.put_org_environment(&org_environment(
            "acme",
            "prod-2",
            5,
            vec![source("argocd", "prod")],
            3_000,
        ))
        .await,
        Err(OrgEnvironmentError::DeploymentSourceTaken { .. })
    ));
    assert_eq!(
        db.get_org_environment("acme", "prod-2").await.unwrap(),
        None
    );

    tdb.cleanup().await;
}

#[tokio::test]
async fn an_invalid_environment_is_refused_before_anything_is_written() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let twice = org_environment(
        "acme",
        "prod",
        5,
        vec![source("argocd", "prod"), source("argocd", "prod")],
        1_000,
    );
    assert!(matches!(
        db.put_org_environment(&twice).await,
        Err(OrgEnvironmentError::Invalid(_))
    ));
    assert_eq!(db.get_org_environment("acme", "prod").await.unwrap(), None);

    tdb.cleanup().await;
}

#[tokio::test]
async fn delete_takes_the_sources_with_it() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.put_org_environment(&org_environment(
        "acme",
        "prod",
        5,
        vec![source("argocd", "prod")],
        1_000,
    ))
    .await
    .unwrap();

    assert!(db.delete_org_environment("acme", "prod").await.unwrap());
    assert!(
        !db.delete_org_environment("acme", "prod").await.unwrap(),
        "absent the second time"
    );
    assert_eq!(db.get_org_environment("acme", "prod").await.unwrap(), None);
    assert_eq!(
        db.environment_for_deployment_source("acme", "argocd", "prod")
            .await
            .unwrap(),
        None
    );
    // The source is free again.
    db.put_org_environment(&org_environment(
        "acme",
        "production",
        5,
        vec![source("argocd", "prod")],
        2_000,
    ))
    .await
    .unwrap();

    tdb.cleanup().await;
}

#[tokio::test]
async fn a_source_routes_to_exactly_its_environment_by_exact_match() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.put_org_environment(&org_environment(
        "acme",
        "prod",
        5,
        vec![source("argocd", "prod"), source("argocd", "prod-eu")],
        1_000,
    ))
    .await
    .unwrap();
    db.put_org_environment(&org_environment(
        "acme",
        "staging",
        5,
        vec![source("argocd", "staging")],
        1_000,
    ))
    .await
    .unwrap();

    for (installation, grouping, expected) in [
        ("argocd", "prod", Some("prod")),
        ("argocd", "prod-eu", Some("prod")),
        ("argocd", "staging", Some("staging")),
        // Exact match only: no prefix, no case folding, no other installation.
        ("argocd", "prod-e", None),
        ("argocd", "PROD", None),
        ("argocd-2", "prod", None),
    ] {
        assert_eq!(
            db.environment_for_deployment_source("acme", installation, grouping)
                .await
                .unwrap()
                .as_deref(),
            expected,
            "{installation}/{grouping}"
        );
    }

    tdb.cleanup().await;
}

#[tokio::test]
async fn two_orgs_with_the_same_name_and_source_are_independent() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.put_org_environment(&org_environment(
        "acme",
        "prod",
        5,
        vec![source("argocd", "prod")],
        1_000,
    ))
    .await
    .unwrap();
    // Same name, same source, another Org: no collision.
    let globex = db
        .put_org_environment(&org_environment(
            "globex",
            "prod",
            45,
            vec![source("argocd", "prod")],
            1_100,
        ))
        .await
        .unwrap();

    assert_eq!(
        db.environment_for_deployment_source("globex", "argocd", "prod")
            .await
            .unwrap()
            .as_deref(),
        Some("prod")
    );
    assert!(db.delete_org_environment("acme", "prod").await.unwrap());
    assert_eq!(
        db.get_org_environment("globex", "prod").await.unwrap(),
        Some(globex),
        "deleting acme's prod leaves globex's alone"
    );
    assert_eq!(db.list_org_environments("acme").await.unwrap(), vec![]);
    tdb.cleanup().await;
}
