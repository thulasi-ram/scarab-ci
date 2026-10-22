//! PluginInstallations and report ingest (ADR-0072, migration 0055) against
//! *real* Postgres.
//!
//! The per-item rules are pure and tested in `scarab-plugin`; what only
//! Postgres can show is what spans rows and transactions: a repeat key is a
//! duplicate and a reused key with other content rolls the whole batch back,
//! a sequence not newer than the cursor is stale, routing follows the
//! Environment sources as they are *now* (unmapped → mapped, a grouping moved
//! to another Environment), ownership is kept without touching the record,
//! concurrent batches on one target serialize, two Orgs never meet, a kill
//! refuses ingest, an uninstall takes its sources with it and retires its
//! name for good, a target routed somewhere its last status never reached is
//! flagged for a re-send (and a moved one shows where it went), the
//! per-installation caps block only new rows, and lists page by key. Every test gets its own database (`fresh_db`). Skips cleanly
//! when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_deployment::{DeploymentInput, DeploymentStore, SourceAssertedStatus};
use scarab_plugin::{
    EnvironmentTargetsPage, InstallationDeploymentSource, InstallationState, OwnershipReportItem,
    PluginCapability, PluginInstallation, PluginManifest, PluginStore, PluginStoreError,
    ReportIngest, ReportItem, ReportOutcome, ReportedStatus, RuntimeStatus, SourceTargetsPage,
    StatusReportItem, TargetRouting, MAX_TARGETS_PER_INSTALLATION,
    MAX_UNMAPPED_GROUPINGS_PER_INSTALLATION,
};
use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentDeploymentSource, OrgEnvironment, OrgEnvironmentStore,
};

const MINUTE_MS: i64 = 60_000;

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
        capabilities: vec![PluginCapability::ReportDeployments],
        connections: vec![],
        config_keys: vec![],
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
    }
}

async fn put_environment(db: &PostgresDb, org: &str, name: &str, sources: &[(&str, &str)]) {
    db.put_org_environment(&OrgEnvironment {
        org: org.into(),
        name: name.into(),
        description: String::new(),
        deployment_deadline: EnvironmentDeploymentDeadline::new(5 * MINUTE_MS).unwrap(),
        deployment_sources: sources
            .iter()
            .map(|(installation, grouping)| EnvironmentDeploymentSource {
                installation: (*installation).into(),
                vendor_grouping: (*grouping).into(),
            })
            .collect(),
        created_at: 1_000,
        updated_at: 1_000,
        updated_by: "amy".into(),
    })
    .await
    .unwrap();
}

fn status(key: &str, sequence: u64, grouping: &str, target: &str, version: &str) -> ReportItem {
    ReportItem::Status(StatusReportItem {
        idempotency_key: key.into(),
        sequence,
        vendor_grouping: grouping.into(),
        target_id: target.into(),
        version: version.into(),
        version_scheme: "argocd.target-images.v1".into(),
        status: ReportedStatus::Live,
        label: "synced".into(),
        link: None,
        source_at: 7,
        owners: None,
    })
}

/// The content hash is opaque to the store; tests derive a stand-in from the
/// item's JSON so equal items hash equal.
fn ingest(item: ReportItem) -> ReportIngest {
    ReportIngest {
        content_sha256: serde_json::to_string(&item).unwrap(),
        item,
    }
}

async fn outcomes(
    db: &PostgresDb,
    id: &str,
    items: Vec<ReportItem>,
    at: i64,
) -> Vec<ReportOutcome> {
    let items: Vec<ReportIngest> = items.into_iter().map(ingest).collect();
    db.ingest_report(id, &items, at)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.outcome)
        .collect()
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

#[tokio::test]
async fn a_repeat_is_a_duplicate_and_a_reused_key_with_other_content_rolls_back_the_batch() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    put_environment(&db, "acme", "prod", &[("argocd", "prod")]).await;

    let first = db
        .ingest_report(
            "0000000001",
            &[ingest(status("k1", 1, "prod", "argocd/web", "v1"))],
            1_000,
        )
        .await
        .unwrap();
    assert_eq!(first[0].outcome, ReportOutcome::Accepted);
    let seq = first[0].input_seq.expect("a status lands in the record");

    // The same item again: a duplicate pointing at the first copy.
    let again = db
        .ingest_report(
            "0000000001",
            &[ingest(status("k1", 1, "prod", "argocd/web", "v1"))],
            2_000,
        )
        .await
        .unwrap();
    assert_eq!(again[0].outcome, ReportOutcome::Duplicate);
    assert_eq!(again[0].input_seq, Some(seq));

    // A repeat inside ONE batch sees the first copy, though receipts are
    // written only after every item is decided.
    let twice = db
        .ingest_report(
            "0000000001",
            &[
                ingest(status("k9", 9, "prod", "argocd/api", "a9")),
                ingest(status("k9", 9, "prod", "argocd/api", "a9")),
            ],
            2_500,
        )
        .await
        .unwrap();
    assert_eq!(
        (twice[0].outcome, twice[1].outcome),
        (ReportOutcome::Accepted, ReportOutcome::Duplicate)
    );
    assert_eq!(twice[1].input_seq, twice[0].input_seq);
    assert!(twice[0].input_seq.is_some());

    // k1 reused for different content, after a good item: the whole batch
    // is refused, so k2 is not recorded either.
    let err = db
        .ingest_report(
            "0000000001",
            &[
                ingest(status("k2", 2, "prod", "argocd/web", "v2")),
                ingest(status("k1", 3, "prod", "argocd/web", "v3")),
            ],
            3_000,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PluginStoreError::IdempotencyConflict { .. }));
    assert_eq!(
        db.inputs("acme", "prod").await.unwrap().len(),
        2,
        "only k1 and k9 are in the record"
    );
    assert_eq!(
        outcomes(
            &db,
            "0000000001",
            vec![status("k2", 2, "prod", "argocd/web", "v2")],
            4_000
        )
        .await,
        vec![ReportOutcome::Accepted],
        "k2 was rolled back, so it is new"
    );
    tdb.cleanup().await;
}

#[tokio::test]
async fn a_sequence_not_newer_than_the_cursor_is_stale() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    put_environment(&db, "acme", "prod", &[("argocd", "prod")]).await;
    assert_eq!(
        outcomes(
            &db,
            "0000000001",
            vec![
                status("k5", 5, "prod", "argocd/web", "v5"),
                // Same sequence, new key: still not newer.
                status("k5b", 5, "prod", "argocd/web", "v5b"),
                // Older, delivered late.
                status("k3", 3, "prod", "argocd/web", "v3"),
                // Another target has its own cursor.
                status("k1", 1, "prod", "argocd/api", "a1"),
                status("k6", 6, "prod", "argocd/web", "v6"),
            ],
            1_000
        )
        .await,
        vec![
            ReportOutcome::Accepted,
            ReportOutcome::Stale,
            ReportOutcome::Stale,
            ReportOutcome::Accepted,
            ReportOutcome::Accepted,
        ]
    );
    let versions: Vec<String> = db
        .inputs("acme", "prod")
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.input.version().0.clone())
        .collect();
    assert_eq!(versions, vec!["v5", "a1", "v6"]);
    let cursors: Vec<(String, u64)> = db
        .source_targets("0000000001", &SourceTargetsPage::default())
        .await
        .unwrap()
        .into_iter()
        .map(|t| (t.target_id, t.last_sequence))
        .collect();
    assert_eq!(
        cursors,
        vec![("argocd/api".into(), 1), ("argocd/web".into(), 6)]
    );
    tdb.cleanup().await;
}

#[tokio::test]
async fn an_unmapped_grouping_is_counted_until_an_environment_lists_it() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    assert_eq!(
        outcomes(
            &db,
            "0000000001",
            vec![
                status("k1", 1, "prod", "argocd/web", "v1"),
                status("k2", 2, "prod", "argocd/web", "v2"),
            ],
            1_000
        )
        .await,
        vec![ReportOutcome::Unmapped, ReportOutcome::Unmapped]
    );
    assert!(db.environments("acme").await.unwrap().is_empty());
    let unmapped = db.unmapped_groupings("0000000001", None, 0).await.unwrap();
    assert_eq!(unmapped.len(), 1);
    assert_eq!(
        (
            unmapped[0].vendor_grouping.as_str(),
            unmapped[0].reports,
            unmapped[0].last_target_id.as_str()
        ),
        ("prod", 2, "argocd/web")
    );
    // The sequence advanced anyway: a replay of k1's sequence is stale.
    assert_eq!(
        outcomes(
            &db,
            "0000000001",
            vec![status("k1-retry", 1, "prod", "argocd/web", "v1")],
            1_500
        )
        .await,
        vec![ReportOutcome::Stale]
    );

    // Mapped: it drops off the unmapped list at once, and the next report
    // lands in the record.
    put_environment(&db, "acme", "prod", &[("argocd", "prod")]).await;
    assert!(db
        .unmapped_groupings("0000000001", None, 0)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        outcomes(
            &db,
            "0000000001",
            vec![status("k3", 3, "prod", "argocd/web", "v3")],
            2_000
        )
        .await,
        vec![ReportOutcome::Accepted]
    );
    let logged = db.inputs("acme", "prod").await.unwrap();
    assert_eq!(logged.len(), 1);
    match &logged[0].input {
        DeploymentInput::Report { target, report, .. } => {
            assert_eq!(target.source, "argocd", "core set the source");
            assert_eq!(target.id, "argocd/web");
            assert_eq!(report.status, SourceAssertedStatus::Live);
        }
        other => panic!("expected a report, got {other:?}"),
    }
    assert_eq!(
        logged[0].report_source_at,
        Some(7),
        "the source's clock, display-only"
    );
    tdb.cleanup().await;
}

#[tokio::test]
async fn ownership_is_kept_on_the_target_and_never_enters_the_record() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    put_environment(&db, "acme", "prod", &[("argocd", "prod")]).await;
    let ownership = |key: &str, sequence: u64, owners: &[&str]| {
        ReportItem::Ownership(OwnershipReportItem {
            idempotency_key: key.into(),
            sequence,
            vendor_grouping: "prod".into(),
            target_id: "argocd/web".into(),
            owners: owners.iter().map(|o| o.to_string()).collect(),
        })
    };
    assert_eq!(
        outcomes(
            &db,
            "0000000001",
            vec![ownership("o1", 1, &["acme/web", "acme/api"])],
            1_000
        )
        .await,
        vec![ReportOutcome::Accepted]
    );
    assert!(db.inputs("acme", "prod").await.unwrap().is_empty());
    let targets = db
        .environment_targets("acme", "prod", &EnvironmentTargetsPage::default())
        .await
        .unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].owner_projects, vec!["acme/api", "acme/web"]);
    assert_eq!(targets[0].owners_reported_at, Some(1_000));
    assert_eq!(targets[0].version_scheme, None, "no status yet");

    // A status without owners keeps them; one with `[]` clears them.
    outcomes(
        &db,
        "0000000001",
        vec![status("s2", 2, "prod", "argocd/web", "v2")],
        2_000,
    )
    .await;
    let target = &db
        .environment_targets("acme", "prod", &EnvironmentTargetsPage::default())
        .await
        .unwrap()[0];
    assert_eq!(target.owner_projects, vec!["acme/api", "acme/web"]);
    assert_eq!(
        target.version_scheme.as_deref(),
        Some("argocd.target-images.v1")
    );
    assert_eq!(target.last_version.as_deref(), Some("v2"));
    outcomes(&db, "0000000001", vec![ownership("o3", 3, &[])], 3_000).await;
    let target = &db
        .environment_targets("acme", "prod", &EnvironmentTargetsPage::default())
        .await
        .unwrap()[0];
    assert!(target.owner_projects.is_empty());
    assert_eq!(target.last_version.as_deref(), Some("v2"), "kept");
    tdb.cleanup().await;
}

#[tokio::test]
async fn a_grouping_moved_to_another_environment_re_homes_its_next_report() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    put_environment(&db, "acme", "staging", &[("argocd", "apps")]).await;
    outcomes(
        &db,
        "0000000001",
        vec![status("k1", 1, "apps", "argocd/web", "v1")],
        1_000,
    )
    .await;

    // The admin moves the grouping to prod.
    put_environment(&db, "acme", "staging", &[]).await;
    put_environment(&db, "acme", "prod", &[("argocd", "apps")]).await;
    outcomes(
        &db,
        "0000000001",
        vec![status("k2", 2, "apps", "argocd/web", "v2")],
        2_000,
    )
    .await;

    let versions = |env: &'static str| {
        let db = &db;
        async move {
            db.inputs("acme", env)
                .await
                .unwrap()
                .into_iter()
                .map(|l| l.input.version().0.clone())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(versions("staging").await, vec!["v1"], "history stays put");
    assert_eq!(versions("prod").await, vec!["v2"]);
    assert!(db
        .environment_targets("acme", "staging", &EnvironmentTargetsPage::default())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        db.environment_targets("acme", "prod", &EnvironmentTargetsPage::default())
            .await
            .unwrap()[0]
            .last_sequence,
        2,
        "one cursor per target, wherever it routes"
    );
    tdb.cleanup().await;
}

/// Many batches racing on one target: each sequence is accepted at most once
/// and the cursor ends at the highest. The installation lock serializes them;
/// without it two could both read the old cursor and both append.
#[tokio::test]
async fn concurrent_batches_on_one_target_accept_each_sequence_once() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let db = Arc::new(db);
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    put_environment(&db, "acme", "prod", &[("argocd", "prod")]).await;
    const N: u64 = 16;
    let handles: Vec<_> = (1..=N)
        .map(|seq| {
            let db = db.clone();
            tokio::spawn(async move {
                db.ingest_report(
                    "0000000001",
                    &[ingest(status(
                        &format!("k{seq}"),
                        seq,
                        "prod",
                        "argocd/web",
                        &format!("v{seq}"),
                    ))],
                    1_000,
                )
                .await
                .unwrap()[0]
                    .outcome
            })
        })
        .collect();
    let mut accepted = 0;
    for h in handles {
        match h.await.unwrap() {
            ReportOutcome::Accepted => accepted += 1,
            ReportOutcome::Stale => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    let logged = db.inputs("acme", "prod").await.unwrap();
    assert_eq!(logged.len(), accepted, "one input per accepted item");
    // Accepted sequences were strictly increasing in commit order.
    let seqs: Vec<u64> = logged
        .iter()
        .map(|l| l.input.version().0[1..].parse().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    assert_eq!(
        db.source_targets("0000000001", &SourceTargetsPage::default())
            .await
            .unwrap()[0]
            .last_sequence,
        *seqs.last().unwrap()
    );
    tdb.cleanup().await;
}

#[tokio::test]
async fn two_orgs_with_the_same_installation_name_never_meet() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "000000000a"))
        .await
        .unwrap();
    db.install(&installation("initech", "argocd", "000000000b"))
        .await
        .unwrap();
    put_environment(&db, "acme", "prod", &[("argocd", "prod")]).await;
    // initech lists nothing: its report about the same grouping is unmapped,
    // never routed by acme's sources.
    assert_eq!(
        outcomes(
            &db,
            "000000000b",
            vec![status("k1", 1, "prod", "argocd/web", "x")],
            1_000
        )
        .await,
        vec![ReportOutcome::Unmapped]
    );
    assert_eq!(
        outcomes(
            &db,
            "000000000a",
            vec![status("k1", 1, "prod", "argocd/web", "a")],
            1_000
        )
        .await,
        vec![ReportOutcome::Accepted],
        "keys and cursors are per installation"
    );
    assert!(db.environments("initech").await.unwrap().is_empty());
    assert_eq!(db.inputs("acme", "prod").await.unwrap().len(), 1);
    assert!(db
        .environment_targets("initech", "prod", &EnvironmentTargetsPage::default())
        .await
        .unwrap()
        .is_empty());
    tdb.cleanup().await;
}

#[tokio::test]
async fn a_killed_installation_ingests_nothing_until_re_enabled() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    put_environment(&db, "acme", "prod", &[("argocd", "prod")]).await;
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
    let err = db
        .ingest_report(
            "0000000001",
            &[ingest(status("k1", 1, "prod", "argocd/web", "v1"))],
            3_000,
        )
        .await
        .unwrap_err();
    assert_eq!(err, PluginStoreError::Killed);
    assert!(db.inputs("acme", "prod").await.unwrap().is_empty());

    let enabled = db
        .set_installation_state("acme", "argocd", InstallationState::Enabled, "bob", 4_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((enabled.killed_by, enabled.killed_at), (None, None));
    assert_eq!(
        outcomes(
            &db,
            "0000000001",
            vec![status("k1", 1, "prod", "argocd/web", "v1")],
            5_000
        )
        .await,
        vec![ReportOutcome::Accepted],
        "the refused batch left no receipt"
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

/// F1 (c-min audit): an uninstall takes every source naming it and retires
/// the name, so a later installation can never inherit its routing or its
/// Deployment history.
#[tokio::test]
async fn uninstall_removes_its_sources_and_retires_its_name() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    db.install(&installation("acme", "flux", "0000000002"))
        .await
        .unwrap();
    put_environment(&db, "acme", "prod", &[("argocd", "prod"), ("flux", "prod")]).await;
    put_environment(&db, "acme", "staging", &[("argocd", "staging")]).await;
    outcomes(
        &db,
        "0000000001",
        vec![status("k1", 1, "prod", "argocd/web", "v1")],
        1_000,
    )
    .await;
    assert_eq!(
        db.deployment_sources_naming("acme", "argocd")
            .await
            .unwrap(),
        vec![
            InstallationDeploymentSource {
                environment: "prod".into(),
                vendor_grouping: "prod".into()
            },
            InstallationDeploymentSource {
                environment: "staging".into(),
                vendor_grouping: "staging".into()
            },
        ]
    );

    let removed = db
        .uninstall("acme", "argocd", "amy", 2_000)
        .await
        .unwrap()
        .expect("it existed");
    assert_eq!(removed.deployment_sources.len(), 2);
    assert!(removed.connection_credential_refs.is_empty());
    assert!(db
        .deployment_sources_naming("acme", "argocd")
        .await
        .unwrap()
        .is_empty());
    let prod = db
        .get_org_environment("acme", "prod")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        prod.deployment_sources,
        vec![EnvironmentDeploymentSource {
            installation: "flux".into(),
            vendor_grouping: "prod".into()
        }],
        "the other installation's source stays"
    );
    assert_eq!(db.installation("acme", "argocd").await.unwrap(), None);
    assert_eq!(db.installation_by_id("0000000001").await.unwrap(), None);
    assert!(matches!(
        db.ingest_report(
            "0000000001",
            &[ingest(status("k2", 2, "prod", "argocd/web", "v2"))],
            3_000
        )
        .await,
        Err(PluginStoreError::NotFound)
    ));
    // The history stays in the record, under a name nothing can take again.
    assert_eq!(db.inputs("acme", "prod").await.unwrap().len(), 1);
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

#[tokio::test]
async fn receipts_older_than_the_cutoff_are_pruned() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    outcomes(
        &db,
        "0000000001",
        vec![status("old", 1, "g", "t", "v1")],
        1_000,
    )
    .await;
    outcomes(
        &db,
        "0000000001",
        vec![status("new", 2, "g", "t", "v2")],
        9_000,
    )
    .await;
    assert_eq!(db.prune_report_receipts(5_000).await.unwrap(), 1);
    // `old`'s receipt is gone, but its sequence is still behind the cursor.
    assert_eq!(
        outcomes(
            &db,
            "0000000001",
            vec![status("old", 1, "g", "t", "v1")],
            10_000
        )
        .await,
        vec![ReportOutcome::Stale]
    );
    assert_eq!(
        outcomes(
            &db,
            "0000000001",
            vec![status("new", 2, "g", "t", "v2")],
            10_000
        )
        .await,
        vec![ReportOutcome::Duplicate]
    );
    tdb.cleanup().await;
}

fn resend_page() -> SourceTargetsPage {
    SourceTargetsPage {
        resend_only: true,
        ..Default::default()
    }
}

async fn resend_ids(db: &PostgresDb, id: &str) -> Vec<String> {
    db.source_targets(id, &resend_page())
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.target_id)
        .collect()
}

/// P1 (judge-d1): a grouping reported on while unmapped, then mapped, would
/// stay invisible — a reporter that sends transitions only has nothing new
/// to say. `/targets` flags it for a re-send instead (core never fabricates
/// a report); the re-sent status lands on the board and clears the flag.
#[tokio::test]
async fn a_newly_mapped_grouping_is_flagged_until_its_status_is_re_sent() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    outcomes(
        &db,
        "0000000001",
        vec![status("k1", 1, "staging", "argocd/web", "v1")],
        1_000,
    )
    .await;
    assert!(
        resend_ids(&db, "0000000001").await.is_empty(),
        "unmapped: nowhere to re-send to"
    );

    put_environment(&db, "acme", "staging", &[("argocd", "staging")]).await;
    assert_eq!(resend_ids(&db, "0000000001").await, vec!["argocd/web"]);
    let in_view = &db
        .environment_targets("acme", "staging", &EnvironmentTargetsPage::default())
        .await
        .unwrap()[0];
    assert!(in_view.resend);
    assert_eq!(
        in_view.last_status, None,
        "the dropped status is not shown as staging's"
    );
    // The plugin's own seed state still has it.
    assert_eq!(
        db.source_targets("0000000001", &SourceTargetsPage::default())
            .await
            .unwrap()[0]
            .last_status
            .as_deref(),
        Some("live")
    );

    // An ownership report does not clear it: it says nothing about status.
    outcomes(
        &db,
        "0000000001",
        vec![ReportItem::Ownership(OwnershipReportItem {
            idempotency_key: "o2".into(),
            sequence: 2,
            vendor_grouping: "staging".into(),
            target_id: "argocd/web".into(),
            owners: vec!["acme/web".into()],
        })],
        1_500,
    )
    .await;
    assert_eq!(resend_ids(&db, "0000000001").await, vec!["argocd/web"]);

    // The re-send: current state, next sequence, new key.
    assert_eq!(
        outcomes(
            &db,
            "0000000001",
            vec![status("k3-resend", 3, "staging", "argocd/web", "v1")],
            2_000
        )
        .await,
        vec![ReportOutcome::Accepted]
    );
    let logged = db.inputs("acme", "staging").await.unwrap();
    assert_eq!(logged.len(), 1, "on the board now");
    assert_eq!(logged[0].input.version().0, "v1");
    assert!(resend_ids(&db, "0000000001").await.is_empty(), "cleared");
    let in_view = &db
        .environment_targets("acme", "staging", &EnvironmentTargetsPage::default())
        .await
        .unwrap()[0];
    assert!(!in_view.resend);
    assert_eq!(in_view.last_status.as_deref(), Some("live"));
    tdb.cleanup().await;
}

async fn routing(db: &PostgresDb) -> Vec<TargetRouting> {
    db.target_routings("acme", &[("argocd".into(), "argocd/web".into())])
        .await
        .unwrap()
}

/// P2 (judge-d1): an admin moves a grouping from staging to prod. Staging's
/// record is untouched, but the target now routes to prod (the board shows
/// it Moved); prod's view never shows staging's status as its own (F-3),
/// and the target is flagged for a re-send there.
#[tokio::test]
async fn a_re_homed_target_shows_where_it_went_and_carries_no_stale_status() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    put_environment(&db, "acme", "staging", &[("argocd", "apps")]).await;
    outcomes(
        &db,
        "0000000001",
        vec![status("k1", 1, "apps", "argocd/web", "v1")],
        1_000,
    )
    .await;
    assert_eq!(
        routing(&db).await,
        vec![TargetRouting {
            installation: "argocd".into(),
            target_id: "argocd/web".into(),
            environment: Some("staging".into()),
        }]
    );

    put_environment(&db, "acme", "staging", &[]).await;
    put_environment(&db, "acme", "prod", &[("argocd", "apps")]).await;
    assert_eq!(
        routing(&db).await[0].environment.as_deref(),
        Some("prod"),
        "moved, with no report needed"
    );
    assert_eq!(
        db.inputs("acme", "staging").await.unwrap().len(),
        1,
        "history stays"
    );
    let prod = &db
        .environment_targets("acme", "prod", &EnvironmentTargetsPage::default())
        .await
        .unwrap()[0];
    assert_eq!(prod.last_status, None, "staging's status is not prod's");
    assert!(prod.resend);
    assert!(db
        .environment_targets("acme", "staging", &EnvironmentTargetsPage::default())
        .await
        .unwrap()
        .is_empty());

    // An ownership report routed to prod resets the cursor's stored status
    // (red-team F-3): it was staging's fact.
    outcomes(
        &db,
        "0000000001",
        vec![ReportItem::Ownership(OwnershipReportItem {
            idempotency_key: "o2".into(),
            sequence: 2,
            vendor_grouping: "apps".into(),
            target_id: "argocd/web".into(),
            owners: vec!["acme/web".into()],
        })],
        2_000,
    )
    .await;
    let cursor = &db
        .source_targets("0000000001", &SourceTargetsPage::default())
        .await
        .unwrap()[0];
    assert_eq!(
        (
            cursor.last_status.as_deref(),
            cursor.last_version.as_deref()
        ),
        (None, None)
    );
    assert!(cursor.resend, "only a status landing in prod clears it");

    // Unmapped altogether: it routes nowhere, and is not flagged.
    put_environment(&db, "acme", "prod", &[]).await;
    assert_eq!(routing(&db).await[0].environment, None);
    assert!(resend_ids(&db, "0000000001").await.is_empty());
    tdb.cleanup().await;
}

/// P4 (judge-d1, red-team F-2): past a cap, a NEW target or unmapped
/// grouping is rejected (`cap:`, no receipt); a known one keeps reporting.
#[tokio::test]
async fn the_caps_block_only_new_targets_and_new_unmapped_groupings() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    put_environment(&db, "acme", "prod", &[("argocd", "prod")]).await;
    // Fill to one below each cap directly: ingesting ten thousand reports
    // would prove nothing more.
    sqlx::query(
        "INSERT INTO deployment_source_targets
             (installation_id, target_id, org, vendor_grouping, environment, last_sequence,
              updated_at)
         SELECT '0000000001', 'filler/' || g, 'acme', 'prod', 'prod', 1, 1
         FROM generate_series(1, $1) AS g",
    )
    .bind(MAX_TARGETS_PER_INSTALLATION as i64 - 1)
    .execute(&tdb.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO deployment_source_unmapped
             (installation_id, vendor_grouping, org, reports, first_at, last_at, last_target_id)
         SELECT '0000000001', 'unmapped-' || g, 'acme', 1, 1, 1, 'filler/1'
         FROM generate_series(1, $1) AS g",
    )
    .bind(MAX_UNMAPPED_GROUPINGS_PER_INSTALLATION as i64)
    .execute(&tdb.pool)
    .await
    .unwrap();

    let results = db
        .ingest_report(
            "0000000001",
            &[
                // The last free target slot…
                ingest(status("k1", 1, "prod", "argocd/web", "v1")),
                // …so the next new one is over.
                ingest(status("k2", 1, "prod", "argocd/api", "a1")),
                // A known target keeps reporting.
                ingest(status("k3", 2, "prod", "filler/7", "f2")),
                // A known target in a NEW unmapped grouping: over that cap.
                ingest(status("k4", 3, "elsewhere", "filler/7", "f3")),
                // A counted grouping keeps counting.
                ingest(status("k5", 4, "unmapped-1", "filler/7", "f4")),
            ],
            1_000,
        )
        .await
        .unwrap();
    let got: Vec<ReportOutcome> = results.iter().map(|r| r.outcome).collect();
    assert_eq!(
        got,
        vec![
            ReportOutcome::Accepted,
            ReportOutcome::Rejected,
            ReportOutcome::Accepted,
            ReportOutcome::Rejected,
            ReportOutcome::Unmapped,
        ]
    );
    for i in [1, 3] {
        assert!(
            results[i].reason.as_deref().unwrap().starts_with("cap:"),
            "{:?}",
            results[i].reason
        );
    }
    let usage = db.installation_usage("0000000001").await.unwrap();
    assert_eq!(usage.targets, MAX_TARGETS_PER_INSTALLATION);
    assert_eq!(
        usage.unmapped_groupings,
        MAX_UNMAPPED_GROUPINGS_PER_INSTALLATION
    );
    // A rejected item left no receipt: its key is free once there is room.
    sqlx::query("DELETE FROM deployment_source_targets WHERE target_id = 'filler/1'")
        .execute(&tdb.pool)
        .await
        .unwrap();
    assert_eq!(
        outcomes(
            &db,
            "0000000001",
            vec![status("k2", 1, "prod", "argocd/api", "a1")],
            2_000
        )
        .await,
        vec![ReportOutcome::Accepted]
    );
    tdb.cleanup().await;
}

/// Lists page by key: the next page starts after the last row seen.
#[tokio::test]
async fn targets_and_unmapped_groupings_page_by_key() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("acme", "argocd", "0000000001"))
        .await
        .unwrap();
    put_environment(&db, "acme", "prod", &[("argocd", "prod")]).await;
    outcomes(
        &db,
        "0000000001",
        vec![
            status("k1", 1, "prod", "c", "v"),
            status("k2", 1, "prod", "a", "v"),
            status("k3", 1, "prod", "b", "v"),
            status("k4", 1, "x", "d", "v"),
            status("k5", 1, "y", "e", "v"),
        ],
        1_000,
    )
    .await;
    let page = |after: Option<&str>| SourceTargetsPage {
        after: after.map(String::from),
        limit: 2,
        resend_only: false,
    };
    let ids = |ts: Vec<scarab_plugin::DeploymentSourceTarget>| {
        ts.into_iter().map(|t| t.target_id).collect::<Vec<_>>()
    };
    assert_eq!(
        ids(db.source_targets("0000000001", &page(None)).await.unwrap()),
        vec!["a", "b"]
    );
    assert_eq!(
        ids(db
            .source_targets("0000000001", &page(Some("b")))
            .await
            .unwrap()),
        vec!["c", "d"]
    );
    let env_page = EnvironmentTargetsPage {
        after: Some(("argocd".into(), "a".into())),
        limit: 1,
    };
    assert_eq!(
        ids(db
            .environment_targets("acme", "prod", &env_page)
            .await
            .unwrap()),
        vec!["b"]
    );
    let groupings: Vec<String> = db
        .unmapped_groupings("0000000001", Some("x"), 10)
        .await
        .unwrap()
        .into_iter()
        .map(|u| u.vendor_grouping)
        .collect();
    assert_eq!(groupings, vec!["y"]);
    tdb.cleanup().await;
}
