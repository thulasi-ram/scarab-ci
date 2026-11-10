//! Migration 0090 against real Postgres: the clone library's trust moves
//! from `library_versions.first_party` into `library_trusted_sources`, and
//! the first-party column and the defaults table go.
//!
//! A database migrated up to just before 0090 is seeded with four orgs:
//! `acme` holds two first-party clone versions from one source, `globex`
//! holds first-party clone versions read from two mirrors, `initech` holds
//! a first-party library that is not the clone library and a library named
//! `thulasi-ram/clone` that is not first-party, and `umbrella` holds the
//! first-party clone library with its source already trusted by an admin.
//! Then 0090 runs. `acme` gets one trust row and `globex` one per source,
//! each without its `@<commit>`; `initech` gets none; `umbrella` keeps its
//! admin's row. Installations are untouched. The store then reads the rows
//! back through its own code. Skips cleanly when `SCARAB_TEST_DATABASE_URL`
//! is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_policy::library::{LibraryStore, TrustedSource};
use sqlx::PgPool;

const VERSION: i64 = 90;
const BY: &str = "scarab:migration-0090";
const C1: &str = "1111111111111111111111111111111111111111";
const C2: &str = "2222222222222222222222222222222222222222";
const CLONE: &str = "github.com/thulasi-ram/scarab-ci//libs/clone.yaml";

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

/// A 64-hex content hash made of `c`.
fn hash(c: char) -> String {
    c.to_string().repeat(64)
}

/// Stores a version as the schema before 0090 has it.
async fn version(
    pool: &PgPool,
    org: &str,
    hash: &str,
    source: &str,
    publisher: &str,
    name: &str,
    first_party: bool,
) {
    sqlx::query(
        "INSERT INTO library_versions \
         (org, content_hash, content, source, publisher, name, first_party, fetched_by, fetched_at) \
         VALUES ($1, $2, 'kind: library', $3, $4, $5, $6, 'user:amy', 1)",
    )
    .bind(org)
    .bind(hash)
    .bind(source)
    .bind(publisher)
    .bind(name)
    .bind(first_party)
    .execute(pool)
    .await
    .expect("seed version");
}

async fn install(pool: &PgPool, org: &str, publisher: &str, name: &str, hash: &str) {
    sqlx::query(
        "INSERT INTO library_installations \
         (org, publisher, name, content_hash, installed_by, installed_at, step_up_ref) \
         VALUES ($1, $2, $3, $4, 'user:amy', 2, 'su-1')",
    )
    .bind(org)
    .bind(publisher)
    .bind(name)
    .bind(hash)
    .execute(pool)
    .await
    .expect("seed installation");
}

async fn installations(pool: &PgPool) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT org, publisher, name, content_hash FROM library_installations \
         ORDER BY org, publisher, name",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_clone_librarys_trust_becomes_a_trusted_source_row_per_source() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;

    // acme: the first-party clone library at two commits of one source.
    version(
        &pool,
        "acme",
        &hash('a'),
        &format!("{CLONE}@{C1}"),
        "thulasi-ram",
        "clone",
        true,
    )
    .await;
    version(
        &pool,
        "acme",
        &hash('b'),
        &format!("{CLONE}@{C2}"),
        "thulasi-ram",
        "clone",
        true,
    )
    .await;
    install(&pool, "acme", "thulasi-ram", "clone", &hash('b')).await;
    // globex: the first-party clone library read from two mirrors.
    let mirror_gh = "github.com/mirror/scarab-ci//libs/clone.yaml";
    let mirror_fj = "git.example.com/mirrors/scarab-ci//libs/clone.yaml";
    version(
        &pool,
        "globex",
        &hash('a'),
        &format!("{mirror_gh}@{C1}"),
        "thulasi-ram",
        "clone",
        true,
    )
    .await;
    version(
        &pool,
        "globex",
        &hash('b'),
        &format!("{mirror_fj}@{C2}"),
        "thulasi-ram",
        "clone",
        true,
    )
    .await;
    install(&pool, "globex", "thulasi-ram", "clone", &hash('a')).await;
    // initech: a first-party library that is not the clone library, and a
    // `thulasi-ram/clone` that is not first-party.
    version(
        &pool,
        "initech",
        &hash('c'),
        &format!("github.com/thulasi-ram/scarab-ci//libs/deploy.yaml@{C1}"),
        "thulasi-ram",
        "deploy",
        true,
    )
    .await;
    version(
        &pool,
        "initech",
        &hash('d'),
        &format!("github.com/mallory/scarab-ci//libs/clone.yaml@{C1}"),
        "thulasi-ram",
        "clone",
        false,
    )
    .await;
    install(&pool, "initech", "thulasi-ram", "deploy", &hash('c')).await;
    install(&pool, "initech", "thulasi-ram", "clone", &hash('d')).await;
    // umbrella: the first-party clone library, its source already trusted.
    version(
        &pool,
        "umbrella",
        &hash('a'),
        &format!("{CLONE}@{C1}"),
        "thulasi-ram",
        "clone",
        true,
    )
    .await;
    install(&pool, "umbrella", "thulasi-ram", "clone", &hash('a')).await;
    sqlx::query(
        "INSERT INTO library_trusted_sources (org, source, trusted_by, trusted_at, step_up_ref) \
         VALUES ('umbrella', $1, 'user:dana', 5, 'su-9')",
    )
    .bind(CLONE)
    .execute(&pool)
    .await
    .expect("seed trust");
    sqlx::raw_sql(
        "INSERT INTO library_org_defaults (org, state, updated_at) VALUES \
         ('acme', 'installed', 3), ('globex', 'missing', 3), ('initech', 'offered', 0)",
    )
    .execute(&pool)
    .await
    .expect("seed defaults");
    let installed_before = installations(&pool).await;

    apply(&pool, |v| v >= VERSION).await;

    let rows: Vec<(String, String, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT org, source, trusted_by, trusted_at, step_up_ref FROM library_trusted_sources \
         ORDER BY org, source",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let got: Vec<(&str, &str, &str, Option<&str>)> = rows
        .iter()
        .map(|(o, s, b, _, r)| (o.as_str(), s.as_str(), b.as_str(), r.as_deref()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("acme", CLONE, BY, None),
            ("globex", mirror_fj, BY, None),
            ("globex", mirror_gh, BY, None),
            ("umbrella", CLONE, "user:dana", Some("su-9")),
        ]
    );
    assert!(
        rows.iter().filter(|r| r.2 == BY).all(|r| r.3 > 0),
        "a migration row records when it was made: {rows:?}"
    );
    assert_eq!(rows[3].3, 5, "the admin's row is kept as it was");

    assert_eq!(installations(&pool).await, installed_before);
    let defaults: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('library_org_defaults')::TEXT")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(defaults, None, "library_org_defaults is dropped");
    let first_party: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_name = 'library_versions' AND column_name = 'first_party'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(first_party, 0, "library_versions.first_party is dropped");

    // The store works on the migrated schema.
    let db = PostgresDb::with_pool(pool.clone());
    let sources = |ts: Vec<TrustedSource>| -> Vec<String> {
        let mut s: Vec<String> = ts.into_iter().map(|t| t.source).collect();
        s.sort();
        s
    };
    assert_eq!(
        sources(db.trusted_sources("acme").await.unwrap()),
        vec![CLONE.to_string()]
    );
    assert_eq!(
        sources(db.trusted_sources("globex").await.unwrap()),
        vec![mirror_fj.to_string(), mirror_gh.to_string()]
    );
    assert!(db.trusted_sources("initech").await.unwrap().is_empty());
    let umbrella = db.trusted_sources("umbrella").await.unwrap();
    assert_eq!(umbrella.len(), 1);
    assert_eq!(umbrella[0].trusted_by, "user:dana");
    let acme = db.installations("acme").await.unwrap();
    assert_eq!(acme.len(), 1);
    assert_eq!(acme[0].qualified_name(), "thulasi-ram/clone");
    assert_eq!(acme[0].content_hash, hash('b'));
    assert_eq!(acme[0].source, format!("{CLONE}@{C2}"));
    assert_eq!(db.installations("initech").await.unwrap().len(), 2);
    let v = db.version("globex", &hash('b')).await.unwrap().unwrap();
    assert_eq!(v.source, format!("{mirror_fj}@{C2}"));

    tdb.cleanup().await;
}
