//! Migration 0111 (PrincipalToken kinds) against real Postgres, over a token
//! a 0.8.3 server stored, and `rollback/0111_revert.sql` back again. Skips
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_identity::{api_token_hash, ApiToken, ApiTokenStore, PrincipalTokenKind, Role, Scope};
use sqlx::PgPool;

const REVERT: &str = include_str!("../rollback/0111_revert.sql");
const ADDED: [&str; 4] = ["kind", "operations", "audience", "oauth_client"];

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

async fn api_tokens_columns(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_name = 'api_tokens' ORDER BY ordinal_position",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn token(id: &str) -> ApiToken {
    ApiToken {
        id: id.into(),
        name: id.into(),
        owner_subject: "alice".into(),
        scope: Scope::Org("acme".into()),
        role: Role::Member,
        expires_at: i64::MAX / 2,
        created_by: "alice".into(),
        created_at: 1_000,
        last_used_at: None,
        use_count: 0,
        revoked_at: None,
        kind: PrincipalTokenKind::Issued,
        operations: None,
        audience: None,
        oauth_client: None,
    }
}

/// M1: a token a 0.8.3 server stored reads after 0111 as an issued token
/// with no list, so it works as it did; a 0.8.3 replica's insert during the
/// rolling update still works.
/// Kills: a NOT NULL column without a default, or a default other than
/// `issued` (every existing token would stop authenticating).
#[tokio::test]
async fn a_0_8_3_token_reads_after_0111_as_issued_with_no_list() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let insert = |id: &'static str, plaintext: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO api_tokens (id, token_hash, name, owner_subject, scope_org,
                                         scope_project, role, expires_at, created_by, created_at)
                 VALUES ($1, $2, $1, 'alice', 'acme', '', 'member', 4611686018427387903,
                         'alice', 1000)",
            )
            .bind(id)
            .bind(api_token_hash(plaintext))
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    apply(&pool, |v| v < 111).await;
    insert("t-old", "scarab_pat_old").await;
    apply(&pool, |v| v >= 111).await;
    insert("t-mixed", "scarab_pat_mixed").await;

    let db = PostgresDb::with_pool(pool.clone());
    for (id, plaintext) in [("t-old", "scarab_pat_old"), ("t-mixed", "scarab_pat_mixed")] {
        let read = db
            .by_hash(
                &api_token_hash(plaintext),
                &scarab_identity::restricted_api_token_hash(plaintext),
            )
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{id} reads"));
        assert_eq!(read.kind, PrincipalTokenKind::Issued, "{id}");
        assert_eq!(read.operations, None, "{id}");
        assert_eq!(read.audience, None, "{id}");
        assert_eq!(read.oauth_client, None, "{id}");
    }
    tdb.cleanup().await;
}

/// M2: the revert revokes every token a 0.8.3 server would let do more than
/// it may (one with a list, an `oauth` one, one with an audience), keeps a
/// plain issued token live, drops the four columns and the migration row,
/// runs twice, and the migrator upgrades again.
/// Kills: a revert that drops the list without revoking its token (on 0.8.3
/// it could use every operation its role allows), or one that misses a
/// column or the migration row.
#[tokio::test]
async fn the_0111_revert_revokes_limited_tokens_and_runs_twice() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let db = PostgresDb::with_pool(pool.clone());
    db.migrate().await.unwrap();

    for t in [
        token("t-plain"),
        ApiToken {
            operations: Some(vec!["run.read".into()]),
            ..token("t-listed")
        },
        ApiToken {
            kind: PrincipalTokenKind::Oauth,
            oauth_client: Some("client-1".into()),
            ..token("t-oauth")
        },
        ApiToken {
            audience: Some("https://ci.example.com/mcp".into()),
            ..token("t-audience")
        },
    ] {
        db.put(&t, &t.stored_hash(&format!("scarab_pat_{}", t.id)))
            .await
            .unwrap();
    }

    sqlx::raw_sql(REVERT).execute(&pool).await.unwrap();
    sqlx::raw_sql(REVERT)
        .execute(&pool)
        .await
        .expect("the revert runs twice");

    let cols = api_tokens_columns(&pool).await;
    for c in ADDED {
        assert!(
            !cols.iter().any(|x| x == c),
            "{c} left after the revert: {cols:?}"
        );
    }
    // Version 111 is gone; a later migration (one this branch's successors
    // add) may still be recorded, so the newest version is not asserted.
    let has_0111: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = 111)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!has_0111, "migration 111 still recorded after the revert");
    let revoked: Vec<(String, bool)> =
        sqlx::query_as("SELECT id, revoked_at IS NOT NULL FROM api_tokens ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        revoked,
        [
            ("t-audience".to_string(), true),
            ("t-listed".to_string(), true),
            ("t-oauth".to_string(), true),
            ("t-plain".to_string(), false),
        ]
    );

    db.migrate().await.unwrap();
    let plain = db
        .by_hash(
            &api_token_hash("scarab_pat_t-plain"),
            &scarab_identity::restricted_api_token_hash("scarab_pat_t-plain"),
        )
        .await
        .unwrap()
        .expect("the plain token reads again");
    assert!(plain.is_live(2_000));
    assert_eq!(plain.kind, PrincipalTokenKind::Issued);
    assert_eq!(plain.operations, None);
    tdb.cleanup().await;
}
