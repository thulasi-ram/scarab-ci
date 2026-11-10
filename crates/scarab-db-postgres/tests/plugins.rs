//! PluginInstallations (ADR-0072, migration 0055) against *real* Postgres.
//!
//! The per-field rules are pure and tested in `scarab-plugin`; what only
//! Postgres can show is what spans rows: names unique per Org, a kill that
//! records who and when (and an enable that clears it), the runtime record
//! written only in the state it was decided in, and an uninstall that retires
//! its name for good, and an upgrade written only over the manifest it was
//! judged against (migration 0091). Report ingest went with the Deployment record
//! (ADR-0074). Every test gets its own database (`fresh_db`). Skips cleanly
//! when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeMap;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_plugin::{
    ConnectionSlot, InstallationState, PluginCapability, PluginConnection,
    PluginConnectionAuthScheme, PluginInstallation, PluginManifest, PluginStorage, PluginStore,
    PluginStoreError, PluginUpgrade, RequestRule, RuntimeStatus,
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
        image: None,
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
        source: None,
        source_connection: None,
        config: BTreeMap::new(),
        state: InstallationState::Enabled,
        state_version: 0,
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
            0,
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
            0,
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
            1,
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
    // git-bug da0af59: a pass that read `enabled` before the kill, and
    // writes after the re-enable, finds the state as it read it but not the
    // version: the probe the enable cleared does not come back.
    assert_eq!(enabled.state_version, 2, "killed, then enabled");
    assert!(!db
        .set_runtime_status(
            "0000000001",
            InstallationState::Enabled,
            0,
            RuntimeStatus::Running,
            "",
            Some(2_000)
        )
        .await
        .unwrap());
    let after = db.installation_by_id("0000000001").await.unwrap().unwrap();
    assert_eq!(
        (after.runtime_status, after.probe_passed_at),
        (RuntimeStatus::Pending, None)
    );
    // Enabling one already enabled resets nothing.
    db.set_runtime_status(
        "0000000002",
        InstallationState::Enabled,
        0,
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

fn slot(name: &str) -> ConnectionSlot {
    ConnectionSlot {
        name: name.into(),
        auth_schemes: vec!["bearer".into()],
        read_rules: vec![RequestRule {
            method: "GET".into(),
            path: "/api/version".into(),
            query: vec![],
        }],
    }
}

/// An upgrade is written only over the manifest the caller judged it
/// against, keeps the installation's id, config it still declares, storage,
/// state and the bindings of slots it still declares, resets the runtime
/// record so isolation is proved again, and records the version it
/// replaced. A refused upgrade writes nothing.
#[tokio::test]
async fn an_upgrade_is_conditional_on_the_manifest_and_records_the_version_it_replaced() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let mut argo = installation("acme", "argocd", "0000000001");
    argo.manifest.connections.push(slot("github"));
    argo.manifest.config_keys = vec!["ARGOCD_UI_URL".into(), "OLD_KEY".into()];
    argo.config = BTreeMap::from([
        ("ARGOCD_UI_URL".to_string(), "https://argo".to_string()),
        ("OLD_KEY".to_string(), "x".to_string()),
    ]);
    argo.source = Some(format!(
        "github.com/acme/argocd//manifest.json@{}",
        "1".repeat(40)
    ));
    argo.storage = Some(PluginStorage::new("argocd-state", None).unwrap());
    db.install(&argo).await.unwrap();
    db.set_runtime_status(
        &argo.id,
        InstallationState::Enabled,
        0,
        RuntimeStatus::Running,
        "",
        Some(5_000),
    )
    .await
    .unwrap();
    for (slot, credential_ref) in [("argocd", "pc-argocd"), ("github", "pc-github")] {
        db.put_connection(&PluginConnection {
            installation_id: argo.id.clone(),
            slot: slot.into(),
            base_url: "https://example.test".into(),
            auth_scheme: PluginConnectionAuthScheme::Bearer,
            credential_ref: credential_ref.into(),
            ca_pem: None,
            updated_by: "amy".into(),
            updated_at: 2_000,
        })
        .await
        .unwrap();
    }

    // The new manifest drops the `github` slot and the `OLD_KEY` config key.
    let mut next = argo.manifest.clone();
    next.connections.retain(|s| s.name == "argocd");
    next.config_keys = vec!["ARGOCD_UI_URL".into()];
    let upgrade = PluginUpgrade {
        image: format!("ghcr.io/acme/argocd@sha256:{}", "b".repeat(64)),
        manifest: next.clone(),
        manifest_sha256: "1".repeat(64),
        source: format!("github.com/acme/argocd//manifest.json@{}", "2".repeat(40)),
        source_connection: Some("forge-1".into()),
        approved_by: "bob".into(),
        approved_at: 9_000,
        step_up_ref: None,
    };

    // Judged against another manifest: refused, nothing written.
    assert_eq!(
        db.upgrade_installation("acme", "argocd", &"f".repeat(64), &upgrade)
            .await,
        Err(PluginStoreError::Changed("argocd".into()))
    );
    let unchanged = db.installation("acme", "argocd").await.unwrap().unwrap();
    assert_eq!(unchanged.manifest, argo.manifest);
    assert_eq!(db.connections(&argo.id).await.unwrap().len(), 2);
    assert_eq!(db.installation_versions(&argo.id).await.unwrap(), vec![]);
    assert_eq!(
        db.upgrade_installation("acme", "nope", &argo.manifest_sha256, &upgrade)
            .await,
        Ok(None)
    );

    let done = db
        .upgrade_installation("acme", "argocd", &argo.manifest_sha256, &upgrade)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        done.connection_credential_refs,
        vec!["pc-github".to_string()]
    );
    let now = done.installation;
    assert_eq!(
        now,
        db.installation("acme", "argocd").await.unwrap().unwrap()
    );
    assert_eq!(now.id, argo.id);
    assert_eq!(now.image, upgrade.image);
    assert_eq!(now.manifest, next);
    assert_eq!(now.manifest_sha256, upgrade.manifest_sha256);
    assert_eq!(now.source.as_deref(), Some(upgrade.source.as_str()));
    assert_eq!(now.source_connection.as_deref(), Some("forge-1"));
    assert_eq!(
        now.config,
        BTreeMap::from([("ARGOCD_UI_URL".to_string(), "https://argo".to_string())])
    );
    assert_eq!(now.storage, argo.storage);
    assert_eq!(now.state, InstallationState::Enabled);
    assert_eq!((now.approved_by.as_str(), now.approved_at), ("bob", 9_000));
    // The new image proves isolation again; a status decided for the old
    // one no longer lands.
    assert_eq!(now.runtime_status, RuntimeStatus::Pending);
    assert_eq!(now.probe_passed_at, None);
    assert_eq!(now.state_version, 1);
    assert!(!db
        .set_runtime_status(
            &argo.id,
            InstallationState::Enabled,
            0,
            RuntimeStatus::Running,
            "",
            Some(6_000)
        )
        .await
        .unwrap());
    let slots: Vec<String> = db
        .connections(&argo.id)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.slot)
        .collect();
    assert_eq!(slots, vec!["argocd".to_string()]);

    let history = db.installation_versions(&argo.id).await.unwrap();
    assert_eq!(history.len(), 1);
    let was = &history[0];
    assert_eq!(was.image, argo.image);
    assert_eq!(was.manifest, argo.manifest);
    assert_eq!(was.manifest_sha256, argo.manifest_sha256);
    assert_eq!(was.source, argo.source);
    assert_eq!((was.approved_by.as_str(), was.approved_at), ("amy", 1_000));
    assert_eq!((was.replaced_by.as_str(), was.replaced_at), ("bob", 9_000));

    // The history goes with the installation.
    db.uninstall("acme", "argocd", "amy", 10_000).await.unwrap();
    assert_eq!(db.installation_versions(&argo.id).await.unwrap(), vec![]);
    tdb.cleanup().await;
}
