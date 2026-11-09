//! Seed and read rows of the frozen `legacy_project_environments` table
//! (ADR-0071 §10) on Postgres, for tests only.
//!
//! Migration 0057 renamed the per-Project `environments` table and froze it.
//! Production has no writer for it: the per-repo write API answers 410, and
//! admission reads the org-level governance tables. The split report still
//! reads the legacy rows, and an identity fold rewrites approver ids in them,
//! so a test of either seeds legacy rows here first, in the shape a
//! pre-migration server wrote them.

use scarab_project::{Environment, ProtectionRules};
use sqlx::{PgPool, Row};

/// Create or replace one legacy per-Project environment row, as the retired
/// per-repo `PUT …/environments/{name}` used to write it.
///
/// # Panics
/// On a database error or rules that do not serialise: a test seed that
/// fails should fail the test.
pub async fn put_legacy_project_environment(
    pool: &PgPool,
    org: &str,
    project: &str,
    env: &Environment,
) {
    let protection = serde_json::to_value(&env.protection).expect("legacy rules serialise");
    sqlx::query(
        "INSERT INTO legacy_project_environments (org, project, name, protection)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (org, project, name) DO UPDATE SET protection = EXCLUDED.protection",
    )
    .bind(org)
    .bind(project)
    .bind(&env.name)
    .bind(protection)
    .execute(pool)
    .await
    .expect("seed a legacy project environment");
}

/// One legacy per-Project environment row, read back as its rules.
///
/// # Panics
/// On a database error or a row whose rules do not deserialise.
pub async fn legacy_project_environment(
    pool: &PgPool,
    org: &str,
    project: &str,
    name: &str,
) -> Option<Environment> {
    let row = sqlx::query(
        "SELECT protection FROM legacy_project_environments
         WHERE org = $1 AND project = $2 AND name = $3",
    )
    .bind(org)
    .bind(project)
    .bind(name)
    .fetch_optional(pool)
    .await
    .expect("read a legacy project environment");
    row.map(|r| {
        let protection: ProtectionRules =
            serde_json::from_value(r.get::<serde_json::Value, _>("protection"))
                .expect("legacy rules deserialise");
        Environment {
            name: name.to_string(),
            protection,
        }
    })
}
