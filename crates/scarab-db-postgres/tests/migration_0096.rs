//! Migration 0096 against real Postgres: a library's publisher and name
//! become one name, `<publisher>/<name>`, and an installation made before it
//! keeps its id, its version and its yank.
//!
//! A database migrated up to just before 0096 holds `thulasi-ram/clone`
//! installed in `acme` with its version yanked, and `mirror/clone` in the
//! same org: two installations whose old `name` column is the same. Then
//! 0096 runs, and the store reads the rows back through its own code. Skips
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_policy::library::{LibraryStore, LibraryStoreError, NewInstallation, Replaces};
use sqlx::PgPool;

const VERSION: i64 = 96;
const SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@1111111111111111111111111111111111111111";

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

/// Stores a version and installs it, as the schema before 0096 has them.
async fn seed(pool: &PgPool, publisher: &str, hash: &str) {
    sqlx::query(
        "INSERT INTO library_versions \
         (org, content_hash, content, source, publisher, name, fetched_by, fetched_at) \
         VALUES ('acme', $1, 'steps: []', $2, $3, 'clone', 'user:amy', 1)",
    )
    .bind(hash)
    .bind(SOURCE)
    .bind(publisher)
    .execute(pool)
    .await
    .expect("seed version");
    sqlx::query(
        "INSERT INTO library_installations \
         (org, publisher, name, content_hash, installed_by, installed_at, step_up_ref) \
         VALUES ('acme', $1, 'clone', $2, 'user:amy', 2, 'su-1')",
    )
    .bind(publisher)
    .bind(hash)
    .execute(pool)
    .await
    .expect("seed installation");
}

#[tokio::test]
async fn an_installed_library_keeps_its_id_hash_and_yank_under_publisher_slash_name() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    let clone_hash = "a".repeat(64);
    let mirror_hash = "b".repeat(64);
    seed(&pool, "thulasi-ram", &clone_hash).await;
    seed(&pool, "mirror", &mirror_hash).await;
    sqlx::query(
        "INSERT INTO library_yanks (org, content_hash, reason, yanked_by, yanked_at) \
         VALUES ('acme', $1, 'pins a vulnerable image', 'user:dana', 3)",
    )
    .bind(&clone_hash)
    .execute(&pool)
    .await
    .expect("seed yank");
    let id_before: String = sqlx::query_scalar(
        "SELECT id FROM library_installations WHERE org = 'acme' AND publisher = 'thulasi-ram'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    apply(&pool, |v| v >= VERSION).await;

    let db = PostgresDb::with_pool(pool.clone());
    let clone = db
        .installation("acme", "thulasi-ram/clone")
        .await
        .unwrap()
        .expect("thulasi-ram/clone is still installed");
    assert_eq!(clone.id, id_before);
    assert_eq!(clone.content_hash, clone_hash);
    assert_eq!(clone.source, SOURCE);
    assert_eq!(clone.step_up_ref.as_deref(), Some("su-1"));
    assert_eq!(
        db.installation_by_id("acme", &id_before).await.unwrap(),
        Some(clone.clone())
    );
    let names: Vec<String> = db
        .installations("acme")
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(names, vec!["mirror/clone", "thulasi-ram/clone"]);
    let version = db.version("acme", &clone_hash).await.unwrap().unwrap();
    assert_eq!(version.name, "thulasi-ram/clone");

    let yanks = db.yanked("acme", &[clone_hash.clone()]).await.unwrap();
    assert_eq!(yanks.len(), 1);
    assert_eq!(yanks[0].reason, "pins a vulnerable image");

    // What existing Pipelines invoke is still installed under that name.
    let compile = db.installed_for_compile("acme").await.unwrap();
    assert_eq!(
        compile.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["mirror/clone", "thulasi-ram/clone"]
    );
    assert_eq!(compile["thulasi-ram/clone"].source, SOURCE);

    // The key is (org, name): a second first install under a name is a
    // conflict, and a new name installs.
    let new = |name: &str| NewInstallation {
        org: "acme".into(),
        name: name.into(),
        content_hash: clone_hash.clone(),
        installed_by: "user:amy".into(),
        installed_at: 4,
        step_up_ref: None,
    };
    assert_eq!(
        db.install(&new("thulasi-ram/clone"), &Replaces::Nothing)
            .await
            .unwrap_err(),
        LibraryStoreError::Conflict
    );
    let canonical = db
        .install(&new("thulasi-ram::scarab-ci::clone"), &Replaces::Nothing)
        .await
        .unwrap();
    assert_ne!(canonical.id, id_before);

    tdb.cleanup().await;
}
