//! Migration 0076 (git-bug 19b0a45) against *real* Postgres: existing forge
//! connections are assigned the Org of their bindings, and the Org record is
//! filled.
//!
//! A database migrated up to just before 0076 is seeded with the connections
//! a running install has: one serving one owner, one serving two, one with
//! nothing bound, and a GitHub installation. Then 0076 runs. A connection
//! whose bindings all have one owner is assigned that Org; the others stay
//! unassigned (no row is split, the migration does not refuse); an
//! installation connection gets its installation id; every Org of a binding
//! is in `orgs`. The store then reads the rows back through its own code.
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_forge::ForgeConnectionStore;
use sqlx::PgPool;

const VERSION: i64 = 76;

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

#[tokio::test]
async fn connections_are_assigned_the_org_of_their_bindings() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    sqlx::raw_sql(
        r#"
        INSERT INTO forge_connections (id, kind, base_url, credential_ref) VALUES
            ('fj-acme',            'forgejo', 'https://git.acme.test', 'fj-acme-token'),
            ('fj-shared',          'forgejo', 'https://codeberg.org',  'fj-shared-token'),
            ('fj-empty',           'forgejo', 'https://git.empty.test','fj-empty-token'),
            ('github-install-777', 'github',  'https://api.github.com','github-app');
        INSERT INTO forge_repos (connection_id, owner, name, org, project) VALUES
            ('fj-acme',            'acme',    'web', 'acme',    'web'),
            ('fj-acme',            'acme',    'api', 'acme',    'api'),
            ('fj-shared',          'globex',  'one', 'globex',  'one'),
            ('fj-shared',          'initech', 'two', 'initech', 'two'),
            ('github-install-777', 'umbrella','app', 'umbrella','app');
        "#,
    )
    .execute(&pool)
    .await
    .expect("seed");

    apply(&pool, |v| v == VERSION).await;

    let db = PostgresDb::with_pool(pool.clone());
    let conn = |id: &'static str| {
        let db = &db;
        async move { db.get_connection(id).await.unwrap().unwrap() }
    };
    assert_eq!(conn("fj-acme").await.org.as_deref(), Some("acme"));
    assert_eq!(conn("fj-shared").await.org, None, "two owners: unassigned");
    assert_eq!(conn("fj-empty").await.org, None, "no bindings: unassigned");
    let gh = conn("github-install-777").await;
    assert_eq!(gh.org.as_deref(), Some("umbrella"));
    assert_eq!(gh.installation_id, Some(777));
    assert_eq!(conn("fj-acme").await.installation_id, None);

    // The bindings themselves are untouched.
    assert_eq!(
        db.resolve(&scarab_forge::RepoRef {
            owner: "initech".into(),
            name: "two".into(),
        })
        .await
        .unwrap()
        .unwrap()
        .org,
        "initech"
    );
    assert_eq!(
        db.list_orgs().await.unwrap(),
        vec![
            "acme".to_string(),
            "globex".to_string(),
            "initech".to_string(),
            "umbrella".to_string()
        ]
    );

    tdb.cleanup().await;
}
