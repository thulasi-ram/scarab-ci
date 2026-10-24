//! PG session store lifecycle (ADR-0049 C1) against *real* Postgres: put/get
//! round-trips the principal + CSRF token, put upserts (refresh), delete
//! revokes, and expiry is data the caller judges via `Session::is_valid`.
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{Principal, Role, Session, SessionStore};

fn session(id: &str, subject: &str, expires_at: i64) -> Session {
    Session {
        id: id.into(),
        principal: Principal {
            subject: subject.into(),
            display_name: Some("Alice".into()),
            roles: vec![Role::Member],
        },
        expires_at,
        csrf: format!("csrf-{id}"),
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
        csrf: "rotated".into(),
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
         VALUES ('old', '{\"subject\":\"alice\",\"display_name\":null,\"roles\":[]}', '', 9000)",
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
