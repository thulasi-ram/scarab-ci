//! The minimal org-level Environment (ADR-0071, migration 0054) against
//! *real* Postgres.
//!
//! The rules on one OrgEnvironment are pure and tested in `scarab-project`;
//! what only Postgres can show is what spans rows: a replace swaps the whole
//! source list atomically and keeps `created_at`, a source routes to exactly
//! one Environment per Org (a second claim is refused and changes nothing),
//! deleting an Environment takes its sources with it, and two Orgs never see
//! each other's names or sources, and a source must name a PluginInstallation
//! in the Org — checked in the write's own transaction, so an uninstall racing
//! the write can never leave one behind. Every test gets its own database
//! (`fresh_db`). Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_plugin::{
    InstallationState, PluginCapability, PluginInstallation, PluginManifest, PluginStore,
    RuntimeStatus,
};
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
        // The governance half has its own writes (ADR-0071); a new
        // Environment stores the permissive, fail-closed defaults.
        protection: Default::default(),
        eligibility: Default::default(),
        created_at: at,
        updated_at: at,
        updated_by: "amy".into(),
    }
}

/// The installations these tests' sources name, in every Org they use: a
/// source must name an installation in its Org (c-min F1).
const INSTALLED: [&str; 4] = ["argocd", "argocd-eu", "argocd-prod", "flux"];

fn installation(org: &str, name: &str, id: String) -> PluginInstallation {
    PluginInstallation {
        id,
        org: org.into(),
        name: name.into(),
        plugin: "argocd".into(),
        image: format!("ghcr.io/acme/argocd@sha256:{}", "a".repeat(64)),
        manifest: PluginManifest {
            plugin: "argocd".into(),
            capabilities: vec![PluginCapability::ReportDeployments],
            connections: vec![],
            config_keys: vec![],
            actions: vec![],
        },
        manifest_sha256: "0".repeat(64),
        config: Default::default(),
        state: InstallationState::Enabled,
        runtime_status: RuntimeStatus::Pending,
        runtime_message: String::new(),
        probe_passed_at: None,
        approved_by: "amy".into(),
        approved_at: 1_000,
        killed_by: None,
        killed_at: None,
    }
}

async fn migrated() -> Option<(common::TestDb, PostgresDb)> {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return None;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let mut n = 0;
    for org in ["acme", "globex", "initech"] {
        for name in INSTALLED {
            n += 1;
            db.install(&installation(org, name, format!("{n:0>10}")))
                .await
                .unwrap();
        }
    }
    Some((tdb, db))
}

/// Wait until `n` backends of this database are blocked on a lock — how a
/// test KNOWS a transaction reached its wait, rather than hoping a spawned
/// task got scheduled there.
async fn until_waiting_on_locks(pool: &sqlx::PgPool, n: i64) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting >= n {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{n} lock waiter(s) never appeared"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
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

/// F1 (c-min audit): a source naming no installation in the Org is refused
/// and nothing changes; one an Environment already lists from before the
/// check existed says to remove it, so a deadline-only edit is not a riddle.
#[tokio::test]
async fn a_source_must_name_an_installation_in_the_org() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let err = db
        .put_org_environment(&org_environment(
            "acme",
            "prod",
            5,
            vec![source("ghost", "prod")],
            1_000,
        ))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        OrgEnvironmentError::DeploymentSourceNotInstalled {
            installation: "ghost".into(),
            org: "acme".into(),
            already_listed: false,
        }
    );
    assert!(err.to_string().contains("remove the source"), "{err}");
    assert_eq!(
        db.get_org_environment("acme", "prod").await.unwrap(),
        None,
        "the refused write left no Environment behind"
    );

    // A source written before installations were checked: plant it.
    db.put_org_environment(&org_environment("acme", "prod", 5, vec![], 1_000))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO environment_deployment_sources (org, environment, installation, vendor_grouping)
         VALUES ('acme', 'prod', 'legacy', 'prod')",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();
    let err = db
        .put_org_environment(&org_environment(
            "acme",
            "prod",
            9,
            vec![source("legacy", "prod")],
            2_000,
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            OrgEnvironmentError::DeploymentSourceNotInstalled {
                already_listed: true,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("remove the source"), "{err}");
    // Removing it is the way out.
    db.put_org_environment(&org_environment("acme", "prod", 9, vec![], 3_000))
        .await
        .unwrap();
    tdb.cleanup().await;
}

/// Red-team F-10 / review #9, interleaving CONSTRUCTED: an uninstall holds
/// the installation row (as its DELETE does) while a PUT naming it starts.
/// The PUT waits on its FOR SHARE lock — before touching any source row — and
/// once the uninstall commits it is refused. No deadlock, and no source left
/// naming a retired name.
#[tokio::test]
async fn a_put_racing_an_uninstall_waits_for_it_and_is_refused() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let db = Arc::new(db);
    let mut uninstall = tdb.pool.begin().await.unwrap();
    sqlx::query("DELETE FROM plugin_installations WHERE org = 'acme' AND name = 'flux'")
        .execute(&mut *uninstall)
        .await
        .unwrap();
    let put = {
        let db = db.clone();
        tokio::spawn(async move {
            db.put_org_environment(&org_environment(
                "acme",
                "prod",
                5,
                vec![source("flux", "clusters-prod")],
                1_000,
            ))
            .await
        })
    };
    until_waiting_on_locks(&tdb.pool, 1).await;
    assert!(
        !put.is_finished(),
        "the PUT is held by the uninstall's lock"
    );
    uninstall.commit().await.unwrap();
    let err = put.await.unwrap().unwrap_err();
    assert!(
        matches!(
            &err,
            OrgEnvironmentError::DeploymentSourceNotInstalled { installation, .. }
                if installation == "flux"
        ),
        "{err:?}"
    );
    assert_eq!(
        db.environment_for_deployment_source("acme", "flux", "clusters-prod")
            .await
            .unwrap(),
        None
    );
    tdb.cleanup().await;
}

/// ADR-0073 §3: every loosenable governance write has an `…_expecting` form
/// that writes only if what is stored is still what the caller judged the
/// write against — otherwise `Conflict`, and nothing changes. `None` stays
/// the unconditional write.
#[tokio::test]
async fn an_expecting_write_refuses_when_the_stored_value_moved() {
    use std::num::NonZeroU32;

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

    // Sources: a create expects none; a stale expectation is refused and the
    // replace (description included) rolled back.
    let argo = source("argocd", "prod");
    db.put_org_environment_expecting(
        &org_environment("acme", "prod", 30, vec![argo.clone()], 1),
        Some(&[]),
    )
    .await
    .unwrap();
    conflict(
        db.put_org_environment_expecting(
            &OrgEnvironment {
                description: "changed".into(),
                ..org_environment("acme", "prod", 30, vec![], 2)
            },
            Some(&[]),
        )
        .await,
    );
    let stored = db
        .get_org_environment("acme", "prod")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.deployment_sources, vec![argo.clone()]);
    assert_ne!(stored.description, "changed");
    db.put_org_environment_expecting(
        &org_environment("acme", "prod", 30, vec![], 3),
        Some(&[argo]),
    )
    .await
    .unwrap();

    // Baseline protection.
    let strict = EnvironmentProtectionRules {
        approvers: vec!["amy".into()],
        allowed_refs: vec!["refs/heads/main".into()],
        wait_timer_s: 5,
        require_reason: true,
        deployment_concurrency: NonZeroU32::new(1),
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
        deployment_gate_deadline: Some(EnvironmentDeploymentDeadline::new(2 * MINUTE_MS).unwrap()),
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
        deployment_gate_deadline: None,
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
