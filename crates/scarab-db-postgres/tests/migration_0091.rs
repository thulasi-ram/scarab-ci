//! Migration 0091 against real Postgres: an installation made before the
//! manifest's source was recorded keeps everything it had and reads back
//! with no source, and an upgrade of it records the version it replaced.
//!
//! A database migrated up to just before 0091 holds one installation, written
//! as the schema before 0091 has it. Then 0091 runs, and the store reads the
//! row back through its own code. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_plugin::{PluginStore, PluginUpgrade};
use sqlx::PgPool;

const VERSION: i64 = 91;

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

const MANIFEST: &str = r#"{"plugin":"argocd","capabilities":["use_connection"],
    "connections":[{"name":"argocd","auth_schemes":["bearer"],
                    "read_rules":[{"method":"GET","path":"/api/version"}]}]}"#;

#[tokio::test]
async fn an_installation_from_before_0091_reads_back_with_no_source() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    sqlx::query(
        "INSERT INTO plugin_installation_names (org, name, installation_id, installed_at)
         VALUES ('acme', 'argocd', '0000000001', 1)",
    )
    .execute(&pool)
    .await
    .expect("seed the name");
    sqlx::query(
        "INSERT INTO plugin_installations
             (org, name, id, plugin, image, manifest, manifest_sha256, config, state,
              approved_by, approved_at)
         VALUES ('acme', 'argocd', '0000000001', 'argocd',
                 'ghcr.io/acme/argocd@sha256:aaaa', $1::jsonb, 'sha-before', '{}', 'enabled',
                 'amy', 1)",
    )
    .bind(MANIFEST)
    .execute(&pool)
    .await
    .expect("seed the installation");
    apply(&pool, |v| v >= VERSION).await;

    let db = PostgresDb::with_pool(pool.clone());
    let row = db.installation("acme", "argocd").await.unwrap().unwrap();
    assert_eq!(row.source, None);
    assert_eq!(row.source_connection, None);
    assert_eq!(row.manifest_sha256, "sha-before");
    assert_eq!(row.image, "ghcr.io/acme/argocd@sha256:aaaa");
    assert_eq!(db.installation_versions(&row.id).await.unwrap(), vec![]);

    // An upgrade of it records the version it replaced, with no source.
    let source = format!("github.com/acme/argocd//manifest.json@{}", "1".repeat(40));
    db.upgrade_installation(
        "acme",
        "argocd",
        "sha-before",
        &PluginUpgrade {
            image: "ghcr.io/acme/argocd@sha256:bbbb".into(),
            manifest: row.manifest.clone(),
            manifest_sha256: "sha-after".into(),
            source: source.clone(),
            source_connection: None,
            approved_by: "bob".into(),
            approved_at: 2,
            step_up_ref: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let row = db.installation("acme", "argocd").await.unwrap().unwrap();
    assert_eq!(row.source.as_deref(), Some(source.as_str()));
    let history = db.installation_versions(&row.id).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].source, None);
    assert_eq!(history[0].manifest_sha256, "sha-before");
    tdb.cleanup().await;
}
