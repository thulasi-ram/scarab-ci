//! Migration 0076 (git-bug 19b0a45) against *real* Postgres: existing forge
//! connections are assigned the Org of their bindings, and the Org record is
//! filled.
//!
//! A database migrated up to just before 0076 is seeded with the connections
//! a running install has: one serving one owner, one serving two, one with
//! nothing bound, and a GitHub installation. Then 0076 runs. A connection
//! whose bindings all have one owner is assigned that Org; the others stay
//! unassigned (no row is split, the migration does not refuse); every
//! GitHub installation stays unassigned and gets its installation id; every
//! Org anything is kept for is in `orgs`. The store then reads the rows
//! back through its own code. Skips cleanly when `SCARAB_TEST_DATABASE_URL`
//! is unset.

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
    // One owner, but an installation: its account is not in the database,
    // so its bindings do not decide its Org.
    let gh = conn("github-install-777").await;
    assert_eq!(gh.org, None, "an installation stays unassigned");
    assert_eq!(gh.installation_id, Some(777));
    assert_eq!(gh.account_id, None);
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

/// git-bug 19b0a45: an installation row whose bindings all belong to one
/// owner that is not its account (an old re-sync of installation 5 bound
/// `globex/site`, which installation 9 covers) is not assigned to that
/// owner. Assigned, it would be pinned to installation 5, which does not
/// cover `globex/site`, and globex's own installation could not take the
/// repo back. It stays unassigned, and the repo stays where it is. Before
/// the fix the backfill gave it org `globex`.
#[tokio::test]
async fn an_installation_holding_only_another_owners_repos_stays_unassigned() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    sqlx::raw_sql(
        r#"
        INSERT INTO forge_connections (id, kind, base_url, credential_ref) VALUES
            ('github-install-5', 'github', 'https://api.github.com', 'github-app');
        INSERT INTO forge_repos (connection_id, owner, name, org, project) VALUES
            ('github-install-5', 'globex', 'site', 'globex', 'site');
        "#,
    )
    .execute(&pool)
    .await
    .expect("seed");

    apply(&pool, |v| v == VERSION).await;

    let db = PostgresDb::with_pool(pool.clone());
    let row = db
        .get_connection("github-install-5")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.org, None);
    assert_eq!(row.installation_id, Some(5));
    let site = db
        .resolve(&scarab_forge::RepoRef {
            owner: "globex".into(),
            name: "site".into(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(site.connection.id, "github-install-5");
    assert_eq!(site.org, "globex");

    tdb.cleanup().await;
}

/// A config-declared connection can be named `github-install-` and any
/// number of digits. One too long for a BIGINT does not abort the
/// migration (the upgraded server could not boot); its installation id is
/// left unknown. Before the fix the cast overflowed.
#[tokio::test]
async fn an_installation_id_too_long_for_a_bigint_does_not_abort_the_migration() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    sqlx::raw_sql(
        r#"
        INSERT INTO forge_connections (id, kind, base_url, credential_ref) VALUES
            ('github-install-99999999999999999999', 'github', 'https://api.github.com', 'k'),
            ('github-install-123456789012345678',   'github', 'https://api.github.com', 'k');
        "#,
    )
    .execute(&pool)
    .await
    .expect("seed");

    apply(&pool, |v| v == VERSION).await;

    let db = PostgresDb::with_pool(pool.clone());
    let long = db
        .get_connection("github-install-99999999999999999999")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(long.installation_id, None);
    let max = db
        .get_connection("github-install-123456789012345678")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(max.installation_id, Some(123_456_789_012_345_678));

    tdb.cleanup().await;
}

/// git-bug 19b0a45: an Org exists when anything is kept for it, not only
/// when a repo is bound into it. An Org with members, an API token, a
/// Policy or a secret and no binding is in `orgs` after the upgrade, so an
/// installation of a same-named account does not join it by name. Reserved
/// names (`_forge`, the forge credentials) are not Orgs. Before the fix only
/// bindings and connections filled `orgs`.
#[tokio::test]
async fn the_org_record_is_filled_from_every_org_keyed_table() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    // The secret store keeps its own table; created here as it would be.
    sqlx::raw_sql(
        r#"
        CREATE TABLE secrets (scope TEXT NOT NULL, key TEXT NOT NULL, PRIMARY KEY (scope, key));
        INSERT INTO secrets (scope, key) VALUES
            ('org:initech', 'DEPLOY_KEY'),
            ('repo:hooli/app', 'TOKEN'),
            ('env:vandelay/imports/prod', 'KEY'),
            ('org:_forge', 'github-app');
        INSERT INTO rbac_bindings (subject, org, project, role) VALUES
            ('alice', 'acme', '', 'admin');
        INSERT INTO policies (id, org, project, name, checkpoint, version, body, content_hash,
                              created_at, updated_at)
        VALUES ('p1', 'globex', NULL, 'no-friday', 'run.create', 1, '{}', 'h', 0, 0);
        "#,
    )
    .execute(&pool)
    .await
    .expect("seed");

    apply(&pool, |v| v == VERSION).await;

    let db = PostgresDb::with_pool(pool.clone());
    assert_eq!(
        db.list_orgs().await.unwrap(),
        vec![
            "acme".to_string(),
            "globex".to_string(),
            "hooli".to_string(),
            "initech".to_string(),
            "vandelay".to_string()
        ]
    );

    tdb.cleanup().await;
}
