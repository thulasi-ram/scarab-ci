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
        use_count: 0,
        revoked_at: None,
        kind: scarab_identity::PrincipalTokenKind::Issued,
        operations: None,
        audience: None,
        oauth_client: None,
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
    assert_eq!(db.by_hash("nope", "").await.unwrap(), None);

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
    db.put(&org_tok, &api_token_hash("scarab_pat_org"))
        .await
        .unwrap();
    db.put(&proj_tok, &api_token_hash("scarab_pat_proj"))
        .await
        .unwrap();

    assert_eq!(
        db.by_hash(
            &api_token_hash("scarab_pat_org"),
            &scarab_identity::restricted_api_token_hash("scarab_pat_org")
        )
        .await
        .unwrap(),
        Some(org_tok.clone())
    );
    assert_eq!(
        db.by_hash(
            &api_token_hash("scarab_pat_proj"),
            &scarab_identity::restricted_api_token_hash("scarab_pat_proj")
        )
        .await
        .unwrap(),
        Some(proj_tok.clone())
    );
    // The plaintext itself is not a key into anything.
    assert_eq!(db.by_hash("scarab_pat_org", "").await.unwrap(), None);

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
    // Its calls still count: the late batch adds to the total.
    db.touch("t-org", 5_000, 3).await.unwrap();
    db.touch("t-org", 4_000, 2).await.unwrap();
    let touched = db
        .by_hash(
            &api_token_hash("scarab_pat_org"),
            &scarab_identity::restricted_api_token_hash("scarab_pat_org"),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(touched.last_used_at, Some(5_000));
    assert_eq!(touched.use_count, 5);
    // Touching an unknown id is a no-op, not an error.
    db.touch("no-such-token", 5_000, 1).await.unwrap();

    // By owner: every org, newest first; another owner's token is not there.
    let theirs = ApiToken {
        owner_subject: "bob".into(),
        ..token("t-bob", Scope::Org("acme".into()), 4_000)
    };
    db.put(&theirs, &api_token_hash("scarab_pat_bob"))
        .await
        .unwrap();
    assert_eq!(
        db.list_owned("alice")
            .await
            .unwrap()
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>(),
        vec!["t-other", "t-proj", "t-org"]
    );

    // Revocation: once, honestly. The second call reports false rather than
    // moving the stamp, so when the credential actually died is never rewritten.
    assert!(db.revoke("t-org", 6_000).await.unwrap());
    assert!(!db.revoke("t-org", 7_000).await.unwrap());
    assert!(!db.revoke("no-such-token", 6_000).await.unwrap());
    let revoked = db
        .by_hash(
            &api_token_hash("scarab_pat_org"),
            &scarab_identity::restricted_api_token_hash("scarab_pat_org"),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(revoked.revoked_at, Some(6_000));
    assert!(!revoked.is_live(5_999), "revoked is dead at any time");

    // A revoked token is still LISTED — the row is the audit trail, and a
    // listing that hid it would hide the fact that it ever existed.
    assert_eq!(db.list("acme").await.unwrap().len(), 3);

    tdb.cleanup().await;
}

/// ADR-0073: a StepUpConfirmationRequest's replay re-reads its requesting
/// token by row id — live or not, so the caller can tell "revoked" apart.
#[tokio::test]
async fn a_token_is_found_by_its_row_id_revoked_or_not() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let t = token("t-1", Scope::Org("acme".into()), 1_000);
    db.put(&t, &api_token_hash("scarab_pat_one")).await.unwrap();
    assert_eq!(db.by_id("t-1").await.unwrap(), Some(t.clone()));
    assert_eq!(
        db.by_id("scarab_pat_one").await.unwrap(),
        None,
        "not by plaintext"
    );
    assert_eq!(
        db.by_id(&api_token_hash("scarab_pat_one")).await.unwrap(),
        None
    );

    assert!(db.revoke("t-1", 2_000).await.unwrap());
    let revoked = db.by_id("t-1").await.unwrap().expect("still found");
    assert_eq!(revoked.revoked_at, Some(2_000));
    assert!(!revoked.is_live(1_500));

    tdb.cleanup().await;
}

/// ADR-0086 §6: a PrincipalToken's kind, operation list, audience and OAuth
/// client survive the SQL as the in-memory store keeps them (the server
/// suite runs on that store), and a row whose kind this binary does not know
/// reads as no token at all.
/// Kills: a column left out of the INSERT or the SELECT (a listed token read
/// back as unlimited), and an unknown kind defaulted to `issued`.
#[tokio::test]
async fn a_token_s_kind_and_operation_list_round_trip_and_an_unknown_kind_is_no_token() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let listed = ApiToken {
        operations: Some(vec!["repo.dispatch".into(), "run.read".into()]),
        ..token("t-listed", Scope::Org("acme".into()), 1_000)
    };
    let oauth = ApiToken {
        kind: scarab_identity::PrincipalTokenKind::Oauth,
        audience: Some("https://ci.example.com/mcp".into()),
        oauth_client: Some("client-1".into()),
        ..token("t-oauth", Scope::Org("acme".into()), 2_000)
    };
    let plain = token("t-plain", Scope::Org("acme".into()), 3_000);
    for (t, plaintext) in [
        (&listed, "scarab_pat_listed"),
        (&oauth, "scarab_pat_oauth"),
        (&plain, "scarab_pat_plain"),
    ] {
        // Stored as the mint stores it: a restricted token under its `r1:`
        // hash.
        db.put(t, &t.stored_hash(plaintext)).await.unwrap();
        assert_eq!(
            db.by_hash(
                &api_token_hash(plaintext),
                &scarab_identity::restricted_api_token_hash(plaintext)
            )
            .await
            .unwrap()
            .as_ref(),
            Some(t),
            "{}",
            t.id
        );
    }

    sqlx::query("UPDATE api_tokens SET kind = 'bogus' WHERE id = 't-plain'")
        .execute(&tdb.pool)
        .await
        .unwrap();
    assert_eq!(
        db.by_hash(
            &api_token_hash("scarab_pat_plain"),
            &scarab_identity::restricted_api_token_hash("scarab_pat_plain")
        )
        .await
        .unwrap(),
        None
    );
    assert_eq!(db.by_id("t-plain").await.unwrap(), None);

    tdb.cleanup().await;
}

/// J1 (ADR-0086 §6, rolling upgrade): a restricted token (here one with an
/// operation list) is stored under its `r1:` hash, so a lookup by the plain
/// hash alone, which is all a server built before 0111 makes, finds nothing;
/// the two-hash lookup still finds it, and a plain token is still found by
/// its plain hash. A restricted row stored under a plain hash by mistake is
/// never found.
/// Kills: a mint that stores a listed token under the plain hash (a 0.8.3
/// replica would let it do everything its role allows), and a lookup that
/// matches a row under the hash its restrictions do not call for.
#[tokio::test]
async fn a_listed_token_is_invisible_to_a_plain_hash_lookup() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let listed = ApiToken {
        operations: Some(vec!["run.read".into()]),
        ..token("t-listed", Scope::Org("acme".into()), 1_000)
    };
    let plain = token("t-plain", Scope::Org("acme".into()), 2_000);
    let (listed_secret, plain_secret) = ("scarab_pat_listed_j1", "scarab_pat_plain_j1");
    db.put(&listed, &listed.stored_hash(listed_secret))
        .await
        .unwrap();
    db.put(&plain, &plain.stored_hash(plain_secret))
        .await
        .unwrap();
    assert!(listed
        .stored_hash(listed_secret)
        .starts_with(scarab_identity::RESTRICTED_API_TOKEN_HASH_PREFIX));
    assert_eq!(
        plain.stored_hash(plain_secret),
        api_token_hash(plain_secret)
    );

    // What a pre-0111 server runs: the plain hash, no restriction columns.
    let old_lookup = |secret: &'static str| {
        let pool = tdb.pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT id FROM api_tokens WHERE token_hash = $1")
                .bind(api_token_hash(secret))
                .fetch_optional(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(old_lookup(listed_secret).await, None);
    assert_eq!(old_lookup(plain_secret).await, Some("t-plain".to_string()));

    // This binary: the plain hash alone does not find the listed token; both
    // hashes do.
    assert_eq!(
        db.by_hash(&api_token_hash(listed_secret), "")
            .await
            .unwrap(),
        None
    );
    let both = |secret: &str| {
        (
            api_token_hash(secret),
            scarab_identity::restricted_api_token_hash(secret),
        )
    };
    let (p, r) = both(listed_secret);
    assert_eq!(db.by_hash(&p, &r).await.unwrap(), Some(listed.clone()));
    let (p, r) = both(plain_secret);
    assert_eq!(db.by_hash(&p, &r).await.unwrap(), Some(plain));

    // A restricted row under a plain hash is never found.
    let misfiled = ApiToken {
        operations: Some(vec!["run.read".into()]),
        ..token("t-misfiled", Scope::Org("acme".into()), 3_000)
    };
    db.put(&misfiled, &api_token_hash("scarab_pat_misfiled"))
        .await
        .unwrap();
    let (p, r) = both("scarab_pat_misfiled");
    assert_eq!(db.by_hash(&p, &r).await.unwrap(), None);

    tdb.cleanup().await;
}
