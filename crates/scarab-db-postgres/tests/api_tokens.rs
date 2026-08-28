//! PG issued-API-token store (ADR-0049 amendment) against *real* Postgres.
//!
//! The in-memory fake in `scarab-testkit` is what the server suite runs on, so
//! this is where the SQL itself is held to the same contract: the record round
//! -trips through both scope shapes, lookup is by DIGEST only, listing is
//! org-wide and newest-first, revocation is idempotent-but-honest, and `touch`
//! is monotonic. Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{api_token_hash, ApiToken, ApiTokenStore, Role, Scope};

fn token(id: &str, scope: Scope, created_at: i64) -> ApiToken {
    ApiToken {
        id: id.into(),
        name: format!("token {id}"),
        owner_subject: "alice".into(),
        scope,
        role: Role::Member,
        expires_at: created_at + 10_000,
        created_by: "alice".into(),
        created_at,
        last_used_at: None,
        revoked_at: None,
    }
}

#[tokio::test]
async fn api_token_lifecycle_put_lookup_list_touch_revoke() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    // Unknown digest: no token. (And no way to ask by anything else — the
    // plaintext is not a column.)
    assert_eq!(db.by_hash("nope").await.unwrap(), None);

    // An org-scoped token and a project-scoped one: both scope shapes must
    // survive the `project=''` encoding the table shares with rbac_bindings.
    let org_tok = token("t-org", Scope::Org("acme".into()), 1_000);
    let proj_tok = ApiToken {
        role: Role::Admin,
        ..token(
            "t-proj",
            Scope::Project {
                org: "acme".into(),
                name: "web".into(),
            },
            2_000,
        )
    };
    db.put(&org_tok, &api_token_hash("scarab_pat_org")).await.unwrap();
    db.put(&proj_tok, &api_token_hash("scarab_pat_proj"))
        .await
        .unwrap();

    assert_eq!(
        db.by_hash(&api_token_hash("scarab_pat_org")).await.unwrap(),
        Some(org_tok.clone())
    );
    assert_eq!(
        db.by_hash(&api_token_hash("scarab_pat_proj")).await.unwrap(),
        Some(proj_tok.clone())
    );
    // The plaintext itself is not a key into anything.
    assert_eq!(db.by_hash("scarab_pat_org").await.unwrap(), None);

    // Listing is org-wide (both scope shapes) and newest-first.
    let listed = db.list("acme").await.unwrap();
    assert_eq!(
        listed.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        vec!["t-proj", "t-org"]
    );
    // A token issued in one org is invisible from another.
    let other = token("t-other", Scope::Org("evil".into()), 3_000);
    db.put(&other, &api_token_hash("scarab_pat_other"))
        .await
        .unwrap();
    assert_eq!(db.list("acme").await.unwrap().len(), 2);

    // `touch` is monotonic: a late-arriving earlier stamp cannot rewind it.
    db.touch("t-org", 5_000).await.unwrap();
    db.touch("t-org", 4_000).await.unwrap();
    assert_eq!(
        db.by_hash(&api_token_hash("scarab_pat_org"))
            .await
            .unwrap()
            .unwrap()
            .last_used_at,
        Some(5_000)
    );
    // Touching an unknown id is a no-op, not an error.
    db.touch("no-such-token", 5_000).await.unwrap();

    // Revocation: once, honestly. The second call reports false rather than
    // moving the stamp, so when the credential actually died is never rewritten.
    assert!(db.revoke("t-org", 6_000).await.unwrap());
    assert!(!db.revoke("t-org", 7_000).await.unwrap());
    assert!(!db.revoke("no-such-token", 6_000).await.unwrap());
    let revoked = db
        .by_hash(&api_token_hash("scarab_pat_org"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(revoked.revoked_at, Some(6_000));
    assert!(!revoked.is_live(5_999), "revoked is dead at any time");

    // A revoked token is still LISTED — the row is the audit trail, and a
    // listing that hid it would hide the fact that it ever existed.
    assert_eq!(db.list("acme").await.unwrap().len(), 2);

    tdb.cleanup().await;
}
