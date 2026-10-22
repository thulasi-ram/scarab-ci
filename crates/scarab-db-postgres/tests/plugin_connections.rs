//! PluginConnections and the broker journal (ADR-0072 §5, migration 0056)
//! against *real* Postgres.
//!
//! What only Postgres can show: a re-bind replaces the slot's binding whole
//! and hands back the previous credential key (whose secret the server then
//! deletes); a bind to an installation that is gone stores nothing; an
//! uninstall takes the bindings with it and returns their keys; a scheme
//! other than `bearer` cannot reach a row; the journal keeps entries in
//! order, outlives an uninstall, and is pruned by age. Every test gets its
//! own database (`fresh_db`). Skips cleanly when `SCARAB_TEST_DATABASE_URL`
//! is unset.

mod common;

use std::collections::BTreeMap;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_plugin::{
    BrokerJournalEntry, ConnectionSlot, InstallationState, PluginCapability, PluginConnection,
    PluginConnectionAuthScheme, PluginInstallation, PluginManifest, PluginStore, PluginStoreError,
    RequestRule, RuntimeStatus,
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

fn installation(org: &str, name: &str, id: &str) -> PluginInstallation {
    PluginInstallation {
        id: id.into(),
        org: org.into(),
        name: name.into(),
        plugin: "argocd".into(),
        image: format!("ghcr.io/acme/argocd@sha256:{}", "a".repeat(64)),
        manifest: PluginManifest {
            plugin: "argocd".into(),
            capabilities: vec![
                PluginCapability::ReportDeployments,
                PluginCapability::UseConnection,
            ],
            connections: vec![ConnectionSlot {
                name: "argocd".into(),
                auth_schemes: vec!["bearer".into()],
                read_rules: vec![RequestRule {
                    method: "GET".into(),
                    path: "/api/v1/applications".into(),
                }],
            }],
            config_keys: vec![],
        },
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
    }
}

fn binding(installation_id: &str, base_url: &str, credential_ref: &str) -> PluginConnection {
    PluginConnection {
        installation_id: installation_id.into(),
        slot: "argocd".into(),
        base_url: base_url.into(),
        auth_scheme: PluginConnectionAuthScheme::Bearer,
        credential_ref: credential_ref.into(),
        ca_pem: None,
        updated_by: "amy".into(),
        updated_at: 2_000,
    }
}

fn entry(installation_id: &str, path: &str, started_at: i64) -> BrokerJournalEntry {
    BrokerJournalEntry {
        installation_id: installation_id.into(),
        slot: "argocd".into(),
        method: "GET".into(),
        path: path.into(),
        rule: Some("GET /api/v1/applications".into()),
        status: Some(200),
        bytes: Some(42),
        started_at,
        duration_ms: Some(7),
        denied_reason: None,
    }
}

#[tokio::test]
async fn a_re_bind_replaces_the_binding_whole_and_returns_the_old_key() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    assert_eq!(db.connection("0000000001", "argocd").await.unwrap(), None);

    let first = binding("0000000001", "https://argo.acme.test", "pc-one");
    assert_eq!(db.put_connection(&first).await.unwrap(), None);
    assert_eq!(
        db.connection("0000000001", "argocd").await.unwrap(),
        Some(first.clone())
    );

    // A new destination comes with a new credential key: the old key is
    // handed back so its secret can be deleted, and nothing of the first
    // binding survives on the row.
    let second = PluginConnection {
        ca_pem: Some("-----BEGIN CERTIFICATE-----\n…\n-----END CERTIFICATE-----\n".into()),
        updated_by: "bob".into(),
        updated_at: 3_000,
        ..binding("0000000001", "https://argo.internal.test/argo", "pc-two")
    };
    assert_eq!(
        db.put_connection(&second).await.unwrap(),
        Some("pc-one".into())
    );
    assert_eq!(db.connections("0000000001").await.unwrap(), vec![second]);
    tdb.cleanup().await;
}

#[tokio::test]
async fn a_bind_to_a_gone_installation_stores_nothing() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    assert_eq!(
        db.put_connection(&binding("0000000009", "https://argo.test", "pc-x"))
            .await,
        Err(PluginStoreError::NotFound)
    );
    assert!(db.connections("0000000009").await.unwrap().is_empty());
    // And a malformed binding never reaches the table.
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    assert!(matches!(
        db.put_connection(&binding("0000000001", "ftp://argo.test", "pc-x"))
            .await,
        Err(PluginStoreError::Invalid(_))
    ));
    tdb.cleanup().await;
}

#[tokio::test]
async fn the_schema_admits_bearer_only() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    // The domain cannot spell another scheme; the CHECK is the backstop for
    // anything that writes the table some other way.
    let refused = sqlx::query(
        "INSERT INTO plugin_connections
             (installation_id, slot, base_url, auth_scheme, credential_ref, updated_by, updated_at)
         VALUES ('0000000001', 'argocd', 'https://argo.test', 'basic', 'pc-x', 'amy', 1)",
    )
    .execute(&tdb.pool)
    .await;
    assert!(refused.is_err(), "a non-bearer scheme was stored");
    tdb.cleanup().await;
}

#[tokio::test]
async fn unbind_and_uninstall_return_the_credential_keys() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    db.install(&installation("acme", "flux", "0000000002"))
        .await
        .unwrap();
    db.put_connection(&binding("0000000001", "https://argo.test", "pc-argo"))
        .await
        .unwrap();
    db.put_connection(&binding("0000000002", "https://flux.test", "pc-flux"))
        .await
        .unwrap();

    let removed = db
        .delete_connection("0000000002", "argocd")
        .await
        .unwrap()
        .expect("it was bound");
    assert_eq!(removed.credential_ref, "pc-flux");
    assert_eq!(
        db.delete_connection("0000000002", "argocd").await.unwrap(),
        None
    );

    let uninstalled = db
        .uninstall("acme", "argocd", "amy", 4_000)
        .await
        .unwrap()
        .expect("it existed");
    assert_eq!(uninstalled.connection_credential_refs, vec!["pc-argo"]);
    assert!(db.connections("0000000001").await.unwrap().is_empty());
    tdb.cleanup().await;
}

#[tokio::test]
async fn the_journal_keeps_order_outlives_an_uninstall_and_is_pruned_by_age() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    let refused = BrokerJournalEntry {
        rule: None,
        status: Some(403),
        bytes: None,
        duration_ms: None,
        denied_reason: Some("no read rule admits it".into()),
        ..entry("0000000001", "/api/v1/session", 2_000)
    };
    db.append_broker_journal(&[
        entry("0000000001", "/api/v1/applications", 1_000),
        refused.clone(),
    ])
    .await
    .unwrap();
    db.append_broker_journal(&[entry("0000000002", "/elsewhere", 1_500)])
        .await
        .unwrap();
    db.append_broker_journal(&[]).await.unwrap();

    let newest_first = db.broker_journal("0000000001", 10).await.unwrap();
    assert_eq!(
        newest_first,
        vec![
            refused.clone(),
            entry("0000000001", "/api/v1/applications", 1_000)
        ]
    );
    assert_eq!(db.broker_journal("0000000001", 1).await.unwrap().len(), 1);

    // Audit outlives the installation.
    db.uninstall("acme", "argocd", "amy", 3_000).await.unwrap();
    assert_eq!(db.broker_journal("0000000001", 10).await.unwrap().len(), 2);

    assert_eq!(db.prune_broker_journal(1_600).await.unwrap(), 2);
    assert_eq!(
        db.broker_journal("0000000001", 10).await.unwrap(),
        vec![refused]
    );
    assert!(db
        .broker_journal("0000000002", 10)
        .await
        .unwrap()
        .is_empty());
    tdb.cleanup().await;
}
