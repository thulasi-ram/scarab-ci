//! PG session store lifecycle (ADR-0049 C1) against *real* Postgres: put/get
//! round-trips the principal + CSRF token, put upserts (refresh), delete
//! revokes, and expiry is data the caller judges via `Session::is_valid`.
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_identity::{csrf_hash, session_id_hash, Principal, Role, Session, SessionStore};
use sqlx::{PgPool, Row};

fn session(id: &str, subject: &str, expires_at: i64) -> Session {
    Session {
        id: id.into(),
        principal: Principal {
            subject: subject.into(),
            display_name: Some("Alice".into()),
            roles: vec![Role::Member],
        },
        expires_at,
        csrf_hash: csrf_hash(&format!("csrf-{id}")),
        created_at: 1_000,
        login_kind: scarab_identity::LOGIN_KIND_BROWSER.into(),
    }
}

#[tokio::test]
async fn session_lifecycle_put_get_refresh_delete() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    // Unknown id: no session.
    assert_eq!(db.get("nope").await.unwrap(), None);

    // put → get round-trips the whole session (principal, csrf, expiry).
    let s = session("sid-1", "alice", 10_000);
    db.put(&s).await.unwrap();
    let got = db.get("sid-1").await.unwrap().expect("stored");
    assert_eq!(got, s);
    assert!(got.is_valid(9_999));
    assert!(!got.is_valid(10_000), "expiry is exclusive");

    // put again = upsert (session refresh rotates expiry + csrf).
    let refreshed = Session {
        expires_at: 99_000,
        csrf_hash: csrf_hash("rotated"),
        ..s.clone()
    };
    db.put(&refreshed).await.unwrap();
    assert_eq!(db.get("sid-1").await.unwrap().unwrap(), refreshed);

    // ADR-0073: the login stamp round-trips, and a refresh keeps whatever
    // the caller puts (the server re-puts the session it read).
    assert_eq!(got.created_at, 1_000);
    assert_eq!(got.login_kind, "browser");
    assert_eq!(
        got.fresh_login_until(),
        Some(1_000 + scarab_identity::FRESH_LOGIN_MS)
    );

    // delete revokes; deleting an unknown id is a no-op.
    db.delete("sid-1").await.unwrap();
    assert_eq!(db.get("sid-1").await.unwrap(), None);
    db.delete("sid-1").await.unwrap();

    tdb.cleanup().await;
}

/// ADR-0073: a session row written before migration 0059 (an old binary's
/// INSERT, which never names the new columns) reads back as never fresh.
#[tokio::test]
async fn a_pre_upgrade_session_row_reads_as_never_fresh() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    sqlx::query(
        "INSERT INTO sessions (id, principal, csrf, expires_at)
         VALUES (encode(sha256('old'::bytea), 'hex'),
                 '{\"subject\":\"alice\",\"display_name\":null,\"roles\":[]}', '', 9000)",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();
    let got = db.get("old").await.unwrap().expect("stored");
    assert_eq!(got.created_at, 0);
    assert_eq!(got.login_kind, "");
    assert_eq!(got.fresh_login_until(), None);

    tdb.cleanup().await;
}

// --- git-bug 51d6bcb: no bearer secret at rest -------------------------------

/// The raw `(id, csrf)` of every row in `sessions`.
async fn raw_rows(pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query("SELECT id, csrf FROM sessions ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.get("id"), r.get("csrf")))
        .collect()
}

/// A database read yields nothing that rides a session: the table holds the
/// SHA-256 of the id and of the CSRF token, and the stored key presented as a
/// session id finds nothing.
#[tokio::test]
async fn the_session_table_holds_no_id_or_csrf_token_in_clear() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let s = session("sid-secret", "alice", 10_000);
    db.put(&s).await.unwrap();
    let rows = raw_rows(&tdb.pool).await;
    assert_eq!(
        rows,
        vec![(session_id_hash("sid-secret"), csrf_hash("csrf-sid-secret"))]
    );
    assert!(rows
        .iter()
        .all(|(id, csrf)| !id.contains("sid-secret") && !csrf.contains("csrf-")));

    // The clear id finds it (and the CSRF token it was minted with matches);
    // the stored key does not.
    let got = db.get("sid-secret").await.unwrap().expect("stored");
    assert_eq!(got.id, "sid-secret");
    assert!(got.csrf_matches(Some("csrf-sid-secret")));
    assert!(!got.csrf_matches(Some(&csrf_hash("csrf-sid-secret"))));
    assert_eq!(db.get(&session_id_hash("sid-secret")).await.unwrap(), None);

    // An old binary's clear-text INSERT is refused, not quietly stored.
    let planted = sqlx::query(
        "INSERT INTO sessions (id, principal, csrf, expires_at)
         VALUES ('a-clear-id', '{\"subject\":\"bob\",\"display_name\":null,\"roles\":[]}', '', 9000)",
    )
    .execute(&tdb.pool)
    .await;
    assert!(planted.is_err(), "a clear-text session id was stored");
    let planted = sqlx::query(
        "INSERT INTO sessions (id, principal, csrf, expires_at)
         VALUES (encode(sha256('x'::bytea), 'hex'),
                 '{\"subject\":\"bob\",\"display_name\":null,\"roles\":[]}', 'a-clear-csrf', 9000)",
    )
    .execute(&tdb.pool)
    .await;
    assert!(planted.is_err(), "a clear-text CSRF token was stored");

    tdb.cleanup().await;
}

async fn apply_through(pool: &PgPool, last_version: i64) {
    for m in MIGRATOR.iter().filter(|m| m.version <= last_version) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

/// Migration 0064 converts the sessions already signed in, in place: each
/// browser's cookie and CSRF cookie still work afterwards (nobody is logged
/// out), and no clear-text id or token is left in the table.
#[tokio::test]
async fn migration_0064_hashes_live_sessions_in_place() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let hashing = MIGRATOR
        .iter()
        .find(|m| m.description == "sessions hashed")
        .expect("the hashing migration exists");
    apply_through(&tdb.pool, hashing.version - 1).await;

    // What a pre-upgrade binary wrote: clear id + clear CSRF, and one row
    // with no CSRF token at all.
    for (id, csrf) in [
        (
            "6f1c9a52-0d6e-4f7b-9a43-1d2f3e4a5b6c",
            "8e2d4c6a-1b3f-4e5d-8c7b-6a5f4e3d2c1b",
        ),
        ("legacy-no-csrf", ""),
    ] {
        sqlx::query(
            "INSERT INTO sessions (id, principal, csrf, expires_at, created_at, login_kind)
             VALUES ($1, '{\"subject\":\"alice\",\"display_name\":null,\"roles\":[\"Owner\"]}',
                     $2, 99000, 1000, 'browser')",
        )
        .bind(id)
        .bind(csrf)
        .execute(&tdb.pool)
        .await
        .unwrap();
    }

    sqlx::raw_sql(&hashing.sql)
        .execute(&tdb.pool)
        .await
        .expect("apply the hashing migration");

    let rows = raw_rows(&tdb.pool).await;
    let mut want = vec![
        (
            session_id_hash("6f1c9a52-0d6e-4f7b-9a43-1d2f3e4a5b6c"),
            csrf_hash("8e2d4c6a-1b3f-4e5d-8c7b-6a5f4e3d2c1b"),
        ),
        (session_id_hash("legacy-no-csrf"), String::new()),
    ];
    want.sort();
    assert_eq!(
        rows, want,
        "Postgres sha256() must agree with the Rust digest"
    );

    let db = PostgresDb::with_pool(tdb.pool.clone());
    let got = db
        .get("6f1c9a52-0d6e-4f7b-9a43-1d2f3e4a5b6c")
        .await
        .unwrap()
        .expect("the signed-in browser keeps its session");
    assert_eq!(got.principal.subject, "alice");
    assert_eq!(got.expires_at, 99_000);
    assert!(got.csrf_matches(Some("8e2d4c6a-1b3f-4e5d-8c7b-6a5f4e3d2c1b")));
    let none = db.get("legacy-no-csrf").await.unwrap().expect("kept");
    assert!(!none.csrf_matches(Some("")), "no token matches nothing");

    tdb.cleanup().await;
}
