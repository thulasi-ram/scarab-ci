//! PluginInstallations (ADR-0072, migration 0055) against *real* Postgres.
//!
//! The per-field rules are pure and tested in `scarab-plugin`; what only
//! Postgres can show is what spans rows: names unique per Org, a kill that
//! records who and when (and an enable that clears it), the runtime record
//! written only in the state it was decided in, and an uninstall that retires
//! its name for good. Report ingest went with the Deployment record
//! (ADR-0074). Every test gets its own database (`fresh_db`). Skips cleanly
//! when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeMap;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_plugin::{
    ConnectionSlot, InstallationState, PluginCapability, PluginInstallation, PluginManifest,
    PluginStorage, PluginStore, PluginStoreError, RequestRule, RuntimeStatus,
};

async fn migrated() -> Option<(common::TestDb, PostgresDb)> {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return None;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    Some((tdb, db))
}

fn manifest() -> PluginManifest {
    PluginManifest {
        plugin: "argocd".into(),
        capabilities: vec![PluginCapability::UseConnection],
        connections: vec![ConnectionSlot {
            name: "argocd".into(),
            auth_schemes: vec!["bearer".into()],
            read_rules: vec![RequestRule {
                method: "GET".into(),
                path: "/api/version".into(),
                query: vec![],
            }],
        }],
        config_keys: vec![],
        gates: Default::default(),
    }
}

fn installation(org: &str, name: &str, id: &str) -> PluginInstallation {
    PluginInstallation {
        id: id.into(),
        org: org.into(),
        name: name.into(),
        plugin: "argocd".into(),
        image: format!("ghcr.io/acme/argocd@sha256:{}", "a".repeat(64)),
        manifest: manifest(),
        manifest_sha256: "0".repeat(64),
        config: BTreeMap::new(),
        state: InstallationState::Enabled,
        runtime_status: RuntimeStatus::Pending,
        runtime_message: String::new(),
        probe_passed_at: None,
        approved_by: "amy".into(),
        approved_at: 1_000,
        killed_by: None,
        killed_at: None,
        storage: None,
    }
}

#[tokio::test]
async fn install_round_trips_and_names_are_unique_per_org() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let argo = installation("acme", "argocd", "0000000001");
    assert_eq!(db.install(&argo).await.unwrap(), argo);
    assert_eq!(
        db.installation("acme", "argocd").await.unwrap(),
        Some(argo.clone())
    );
    assert_eq!(
        db.installation_by_id("0000000001").await.unwrap(),
        Some(argo.clone())
    );
    assert!(matches!(
        db.install(&installation("acme", "argocd", "0000000002"))
            .await,
        Err(PluginStoreError::NameTaken(_))
    ));
    assert!(matches!(
        db.install(&installation("acme", "argocd-eu", "0000000001"))
            .await,
        Err(PluginStoreError::IdTaken)
    ));
    // Another Org may use the same name.
    db.install(&installation("initech", "argocd", "0000000003"))
        .await
        .unwrap();
    assert_eq!(db.installations("acme").await.unwrap(), vec![argo]);
    tdb.cleanup().await;
}

/// A manifest approved before ADR-0074 names `report_deployments`: the
/// stored row still reads back, so the installation can be listed, killed
/// and uninstalled.
#[tokio::test]
async fn an_installation_approved_with_report_deployments_still_reads_back() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let mut old = installation("acme", "argocd", "0000000001");
    old.manifest.capabilities = vec![
        PluginCapability::ReportDeployments,
        PluginCapability::UseConnection,
    ];
    db.install(&old).await.unwrap();
    assert_eq!(db.installation("acme", "argocd").await.unwrap(), Some(old));
    tdb.cleanup().await;
}

#[tokio::test]
async fn a_kill_records_who_and_when_and_an_enable_clears_it() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    let killed = db
        .set_installation_state("acme", "argocd", InstallationState::Killed, "amy", 2_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(killed.state, InstallationState::Killed);
    assert_eq!(
        (killed.killed_by.as_deref(), killed.killed_at),
        (Some("amy"), Some(2_000))
    );
    assert_eq!(
        db.installation_by_id("0000000001")
            .await
            .unwrap()
            .unwrap()
            .state,
        InstallationState::Killed,
        "the broker's per-request read sees the kill"
    );
    let enabled = db
        .set_installation_state("acme", "argocd", InstallationState::Enabled, "bob", 4_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((enabled.killed_by, enabled.killed_at), (None, None));
    assert_eq!(
        db.set_installation_state("acme", "nope", InstallationState::Killed, "amy", 5_000)
            .await
            .unwrap(),
        None
    );
    tdb.cleanup().await;
}

/// The plugin runtime's record (slice d2): a status decided for one state is
/// never written over a kill or enable that landed meanwhile, and re-enabling
/// a killed installation forgets its last probe so it is probed again before
/// it starts (ADR-0072 §9).
#[tokio::test]
async fn runtime_status_is_written_only_in_the_state_it_was_decided_in() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    db.install(&installation("globex", "argocd", "0000000002"))
        .await
        .unwrap();
    let ids: Vec<String> = db
        .all_installations()
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, ["0000000001", "0000000002"], "every Org, by id");

    assert!(db
        .set_runtime_status(
            "0000000001",
            InstallationState::Enabled,
            RuntimeStatus::Running,
            "",
            Some(2_000)
        )
        .await
        .unwrap());
    db.set_installation_state("acme", "argocd", InstallationState::Killed, "amy", 3_000)
        .await
        .unwrap();
    // A pass that read `enabled` before the kill must not write `running`
    // over it.
    assert!(!db
        .set_runtime_status(
            "0000000001",
            InstallationState::Enabled,
            RuntimeStatus::Running,
            "",
            Some(2_000)
        )
        .await
        .unwrap());
    let killed = db.installation_by_id("0000000001").await.unwrap().unwrap();
    assert_eq!(
        (killed.runtime_status, killed.probe_passed_at),
        (RuntimeStatus::Running, Some(2_000)),
        "a kill alone leaves the runtime record to the runtime"
    );
    assert!(db
        .set_runtime_status(
            "0000000001",
            InstallationState::Killed,
            RuntimeStatus::Stopped,
            "",
            Some(2_000)
        )
        .await
        .unwrap());

    let enabled = db
        .set_installation_state("acme", "argocd", InstallationState::Enabled, "bob", 4_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            enabled.runtime_status,
            enabled.runtime_message.as_str(),
            enabled.probe_passed_at
        ),
        (RuntimeStatus::Pending, "", None),
        "a re-enabled installation is probed again before it starts"
    );
    // Enabling one already enabled resets nothing.
    db.set_runtime_status(
        "0000000002",
        InstallationState::Enabled,
        RuntimeStatus::Running,
        "",
        Some(5_000),
    )
    .await
    .unwrap();
    let again = db
        .set_installation_state("globex", "argocd", InstallationState::Enabled, "bob", 6_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (again.runtime_status, again.probe_passed_at),
        (RuntimeStatus::Running, Some(5_000))
    );
    tdb.cleanup().await;
}

/// An uninstall retires the name, so a later installation can never inherit
/// what the old one left behind.
#[tokio::test]
async fn uninstall_retires_its_name() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    let removed = db
        .uninstall("acme", "argocd", "amy", 2_000)
        .await
        .unwrap()
        .expect("it existed");
    assert!(removed.connection_credential_refs.is_empty());
    assert_eq!(db.installation("acme", "argocd").await.unwrap(), None);
    assert_eq!(db.installation_by_id("0000000001").await.unwrap(), None);
    assert!(matches!(
        db.install(&installation("acme", "argocd", "0000000003"))
            .await,
        Err(PluginStoreError::NameRetired(_))
    ));
    assert_eq!(
        db.uninstall("acme", "argocd", "amy", 4_000).await.unwrap(),
        None
    );
    // The retired name is per Org.
    db.install(&installation("initech", "argocd", "0000000004"))
        .await
        .unwrap();
    tdb.cleanup().await;
}

/// ADR-0074 §7: the storage write is conditional on the storage the caller
/// read, so a step-up judged against one claim never lands on another
/// admin's newer one; and a half-written storage row cannot exist.
#[tokio::test]
async fn storage_writes_only_over_the_storage_the_caller_read() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    let a = PluginStorage::new("state-a", None).unwrap();
    let b = PluginStorage::new("state-b", Some("/srv/b")).unwrap();

    let set = db
        .set_installation_storage("acme", "argocd", Some(&a), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(set.storage, Some(a.clone()));
    assert_eq!(
        db.installation_by_id("0000000001")
            .await
            .unwrap()
            .unwrap()
            .storage,
        Some(a.clone())
    );
    // Judged against "no storage", but A is there now.
    assert!(matches!(
        db.set_installation_storage("acme", "argocd", Some(&b), None)
            .await,
        Err(PluginStoreError::Changed(_))
    ));
    let moved = db
        .set_installation_storage("acme", "argocd", Some(&b), Some(&a))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.storage, Some(b.clone()));
    // A stale clear is refused too.
    assert!(matches!(
        db.set_installation_storage("acme", "argocd", None, Some(&a))
            .await,
        Err(PluginStoreError::Changed(_))
    ));
    let cleared = db
        .set_installation_storage("acme", "argocd", None, Some(&b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cleared.storage, None);
    // No such installation.
    assert_eq!(
        db.set_installation_storage("acme", "nope", Some(&a), None)
            .await
            .unwrap(),
        None
    );
    // A claim without a mount path cannot be stored.
    let half =
        sqlx::query("UPDATE plugin_installations SET storage_claim = 'x' WHERE id = '0000000001'")
            .execute(&tdb.pool)
            .await;
    assert!(half.is_err(), "the CHECK keeps storage whole");
    tdb.cleanup().await;
}
