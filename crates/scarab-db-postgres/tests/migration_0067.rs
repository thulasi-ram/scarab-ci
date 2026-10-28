//! Migration 0067 (ADR-0074) against *real* Postgres: it drops what
//! migrations 0053 to 0058 added for the Deployment record, and nothing else.
//!
//! A database migrated up to just before 0067 is seeded with a row in every
//! table it drops and in every plugin table it must keep (0055 created both
//! kinds), then 0067 runs. The record's tables and columns are gone; the
//! plugin installation, its connection and its broker journal survive with
//! their rows, the org Environment and its overlay keep their other columns,
//! and the legacy approval-history `deployments` table is untouched. Finally
//! the store reads the surviving rows back through its own code, so the
//! queries match the schema 0067 leaves. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_plugin::PluginStore;
use scarab_project::OrgEnvironmentStore;
use sqlx::{PgPool, Row};

const DROP_VERSION: i64 = 67;

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

async fn table_exists(pool: &PgPool, table: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
        .bind(format!("public.{table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn column_exists(pool: &PgPool, table: &str, column: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2)",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query(&format!("SELECT count(*) AS n FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
        .get("n")
}

/// One row in every table 0067 drops and every plugin table it keeps.
async fn seed(pool: &PgPool) {
    sqlx::raw_sql(
        r#"
        INSERT INTO plugin_installation_names (org, name, installation_id, installed_at)
            VALUES ('acme', 'argo', '00000000a1', 1);
        INSERT INTO plugin_installations
            (org, name, id, plugin, image, manifest, manifest_sha256, state, approved_by, approved_at)
            VALUES ('acme', 'argo', '00000000a1', 'argocd', 'ghcr.io/x/argocd@sha256:aa',
                    '{"plugin":"argocd","capabilities":["report_deployments","use_connection"],
                      "connections":[{"name":"argocd","auth_schemes":["bearer"],
                                      "read_rules":[{"method":"GET","path":"/api/version"}]}]}',
                    'sha', 'enabled', 'user:amy', 1);
        INSERT INTO plugin_connections
            (installation_id, slot, base_url, auth_scheme, credential_ref, updated_by, updated_at)
            VALUES ('00000000a1', 'argocd', 'https://argo.example', 'bearer', 'cred-1', 'user:amy', 1);
        INSERT INTO plugin_broker_journal
            (request_id, phase, installation_id, slot, method, path, started_at)
            VALUES ('r1', 'head', '00000000a1', 'argocd', 'GET', '/api/version', 1);
        INSERT INTO deployment_source_targets
            (installation_id, target_id, org, vendor_grouping, last_sequence, updated_at)
            VALUES ('00000000a1', 'argocd/web', 'acme', 'prod', 1, 1);
        INSERT INTO deployment_source_unmapped
            (installation_id, vendor_grouping, org, reports, first_at, last_at, last_target_id)
            VALUES ('00000000a1', 'dev', 'acme', 1, 1, 1, 'argocd/api');
        INSERT INTO plugin_report_receipts
            (installation_id, idempotency_key, content_sha256, outcome, received_at)
            VALUES ('00000000a1', 'k1', 'sha', 'accepted', 1);
        INSERT INTO org_environments
            (org, name, description, deployment_deadline_ms, created_at, updated_at, updated_by,
             approvers, deployment_concurrency, eligible_all_projects)
            VALUES ('acme', 'prod', 'production', 3600000, 1, 1, 'user:amy',
                    '{user:amy}', 2, true);
        INSERT INTO environment_project_overlays
            (org, environment, project, require_reason, deployment_concurrency,
             deployment_gate_deadline_ms, updated_at, updated_by)
            VALUES ('acme', 'prod', 'web', true, 1, 60000, 1, 'user:amy');
        INSERT INTO environment_deployment_sources (org, environment, installation, vendor_grouping)
            VALUES ('acme', 'prod', 'argo', 'prod');
        INSERT INTO deployment_input_counters (org, last_seq, last_at) VALUES ('acme', 1, 1);
        INSERT INTO deployment_inputs
            (org, seq, environment, target_source, target_id, version, kind, at,
             report_status, report_label, report_source_at)
            VALUES ('acme', 1, 'prod', 'argo', 'argocd/web', 'v1', 'report', 1, 'live', 'synced', 1);
        INSERT INTO deployments (org, project, environment, git_ref, run_id, approved_by, at)
            VALUES ('acme', 'web', 'prod', 'refs/heads/main', 'r1', '["user:amy"]', 1);
        "#,
    )
    .execute(pool)
    .await
    .expect("seed the pre-0067 schema");
}

#[tokio::test]
async fn the_drop_removes_the_record_and_leaves_plugins_and_environments_intact() {
    let Some(tdb) = fresh_db().await else { return };
    let pool = tdb.pool.clone();
    assert!(
        MIGRATOR.iter().any(|m| m.version == DROP_VERSION),
        "migration 0067 exists"
    );
    apply(&pool, |v| v < DROP_VERSION).await;
    seed(&pool).await;
    apply(&pool, |v| v >= DROP_VERSION).await;

    for gone in [
        "run_deployment_requests",
        "deployment_inputs",
        "deployment_input_counters",
        "environment_deployment_sources",
        "deployment_source_targets",
        "deployment_source_unmapped",
        "plugin_report_receipts",
    ] {
        assert!(!table_exists(&pool, gone).await, "{gone} is dropped");
    }
    for (table, column) in [
        ("org_environments", "deployment_deadline_ms"),
        ("org_environments", "deployment_concurrency"),
        ("environment_project_overlays", "deployment_concurrency"),
        (
            "environment_project_overlays",
            "deployment_gate_deadline_ms",
        ),
    ] {
        assert!(
            !column_exists(&pool, table, column).await,
            "{table}.{column} is dropped"
        );
    }

    // Kept, with their rows.
    for (kept, rows) in [
        ("plugin_installation_names", 1),
        ("plugin_installations", 1),
        ("plugin_connections", 1),
        ("plugin_broker_journal", 1),
        ("org_environments", 1),
        ("environment_project_overlays", 1),
        ("deployments", 1),
    ] {
        assert!(table_exists(&pool, kept).await, "{kept} is kept");
        assert_eq!(count(&pool, kept).await, rows, "{kept} keeps its rows");
    }

    // The store reads what survived through its own queries.
    let pg = PostgresDb::with_pool(pool.clone());
    let installation = pg
        .installation("acme", "argo")
        .await
        .expect("the plugin store reads the kept schema")
        .expect("the installation survives");
    assert_eq!(installation.id, "00000000a1");
    assert_eq!(pg.connections("00000000a1").await.unwrap().len(), 1);
    assert_eq!(pg.broker_journal("00000000a1", 10).await.unwrap().len(), 1);
    let env = pg
        .get_org_environment("acme", "prod")
        .await
        .expect("the environment store reads the kept schema")
        .expect("the environment survives");
    assert_eq!(env.description, "production");
    assert_eq!(env.protection.approvers, vec!["user:amy".to_string()]);

    tdb.cleanup().await;
}
