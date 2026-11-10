//! PG identity store (ADR-0069, ADR-0080) against *real* Postgres: a sign-in
//! is pinned by `(provider, external_id)`, a new identity always mints a NEW
//! Principal with a new random id, a handle is never minted twice (nor one
//! that differs only in case), and two racing first sign-ins of one identity
//! converge on one Principal. Skips cleanly when `SCARAB_TEST_DATABASE_URL`
//! is unset.

mod common;

use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{is_principal_id, IdentityStore, Placeholder, ProviderIdentity};

/// The handle of Principal `id`.
async fn handle_of(db: &PostgresDb, id: &str) -> String {
    db.principal(id).await.unwrap().unwrap().handle
}

fn ident(external_id: &str, handle: &str) -> ProviderIdentity {
    ProviderIdentity {
        provider: "github".into(),
        external_id: external_id.into(),
        handle: handle.into(),
        email: None,
        email_verified: false,
        display_name: Some(format!("{handle} (display)")),
    }
}

#[tokio::test]
async fn sign_in_pins_by_external_id_and_never_reuses_an_id() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    // First sign-in mints the base id and claims it.
    let alice = db
        .sign_in(
            &ident("1001", "alice"),
            "github:alice",
            Placeholder::Never,
            10,
        )
        .await
        .unwrap()
        .id;
    assert!(is_principal_id(&alice), "the key is a random id: {alice}");
    assert_eq!(handle_of(&db, &alice).await, "github:alice");
    let rec = db.principal(&alice).await.unwrap().unwrap();
    assert_eq!(rec.claimed_at, Some(10));
    assert_eq!(rec.display_name.as_deref(), Some("alice (display)"));

    // A rename: same external id, new handle → same Principal, handle refreshed.
    let again = db
        .sign_in(
            &ident("1001", "alice-renamed"),
            "github:alice-renamed",
            Placeholder::Never,
            20,
        )
        .await
        .unwrap()
        .id;
    assert_eq!(again, alice);
    let ids = db.identities_of(&alice).await.unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0].handle, "alice-renamed");
    assert_eq!(
        db.principal(&alice).await.unwrap().unwrap().claimed_at,
        Some(10),
        "claimed_at records the FIRST sign-in"
    );

    // A recycled login: the id `github:alice` exists, so the stranger gets the
    // next suffix — even though alice no longer carries that handle.
    let stranger = db
        .sign_in(
            &ident("2002", "alice"),
            "github:alice",
            Placeholder::Never,
            30,
        )
        .await
        .unwrap()
        .id;
    assert_ne!(stranger, alice);
    assert_eq!(handle_of(&db, &stranger).await, "github:alice~2");
    let third = db
        .sign_in(
            &ident("3003", "alice"),
            "github:alice",
            Placeholder::Never,
            40,
        )
        .await
        .unwrap()
        .id;
    assert_eq!(handle_of(&db, &third).await, "github:alice~3");

    // Unknown ids have no record — the store never invents one on read.
    assert_eq!(db.principal("github:nobody").await.unwrap(), None);
}

#[tokio::test]
async fn racing_first_sign_ins_of_one_identity_converge_on_one_principal() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    db.migrate().await.unwrap();

    let mut tasks = Vec::new();
    for _ in 0..16 {
        let db = db.clone();
        tasks.push(tokio::spawn(async move {
            db.sign_in(&ident("4004", "bob"), "github:bob", Placeholder::Never, 1)
                .await
                .unwrap()
                .id
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.push(t.await.unwrap());
    }
    ids.dedup();
    assert_eq!(ids.len(), 1, "one identity, one Principal");
    assert_eq!(handle_of(&db, &ids[0]).await, "github:bob");
    // And the losers' rolled-back mints left no `~N` principals behind.
    assert!(db.by_handle("github:bob~2").await.unwrap().is_empty());
}

/// An import pins an identity WITHOUT claiming it; the sign-in that presents
/// that same identity claims it, and a handle refresh from a later import
/// never touches the contact details a sign-in recorded.
#[tokio::test]
async fn pin_mints_unclaimed_and_sign_in_claims_it() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let carol = db
        .pin(
            &ident("3003", "carol"),
            "github:carol",
            Placeholder::Never,
            5,
        )
        .await
        .unwrap()
        .id;
    assert_eq!(handle_of(&db, &carol).await, "github:carol");
    assert_eq!(
        db.principal(&carol).await.unwrap().unwrap().claimed_at,
        None
    );
    // Re-importing the same account is the same Principal, still unclaimed.
    assert_eq!(
        db.pin(
            &ident("3003", "carol"),
            "github:carol",
            Placeholder::Never,
            6
        )
        .await
        .unwrap()
        .id,
        carol
    );
    assert_eq!(
        db.principal(&carol).await.unwrap().unwrap().claimed_at,
        None
    );

    let signed = ProviderIdentity {
        email: Some("carol@example.com".into()),
        email_verified: true,
        ..ident("3003", "carol")
    };
    assert_eq!(
        db.sign_in(&signed, "github:carol", Placeholder::Never, 7)
            .await
            .unwrap()
            .id,
        carol
    );
    assert_eq!(
        db.principal(&carol).await.unwrap().unwrap().claimed_at,
        Some(7)
    );

    // A later import (no email) refreshes the handle only.
    db.pin(
        &ident("3003", "carol-renamed"),
        "github:carol-renamed",
        Placeholder::Never,
        8,
    )
    .await
    .unwrap()
    .id;
    let held = db.identities_of(&carol).await.unwrap();
    assert_eq!(held[0].handle, "carol-renamed");
    assert_eq!(held[0].email.as_deref(), Some("carol@example.com"));

    // `pinned` finds the Principal by the immutable id alone (a webhook
    // actor's lookup, ADR-0075 §2) and never mints: an unknown account, or
    // a known id on another provider, is none.
    assert_eq!(db.pinned("github", "3003").await.unwrap(), Some(carol));
    assert_eq!(db.pinned("github", "9999").await.unwrap(), None);
    assert_eq!(db.pinned("okta", "3003").await.unwrap(), None);
    assert!(db.principal("github:9999").await.unwrap().is_none());
    tdb.cleanup().await;
}

/// The common trap ADR-0069 §6 exists for: someone who once clicked "Sign in
/// with GitHub" instead of linking now has TWO claimed Principals. Linking the
/// GitHub account (its Principal's only way in) folds that one in — and its
/// API tokens follow the person (capped by the merged-into roles from now on)
/// while its sessions die. A second account on one provider is refused.
#[tokio::test]
async fn link_merges_a_claimed_single_identity_principal_with_its_tokens() {
    use scarab_identity::{
        ApiToken, ApiTokenStore, FoldedPasskey, IdentityError, LinkOutcome, Principal, Scope,
        Session, SessionStore,
    };
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let okta = ProviderIdentity {
        provider: "okta".into(),
        ..ident("00u-ada", "ada@example.com")
    };
    let ada = db
        .sign_in(&okta, "okta:ada@example.com", Placeholder::Never, 1)
        .await
        .unwrap()
        .id;
    let stray = db
        .sign_in(
            &ident("1001", "ada-gh"),
            "github:ada-gh",
            Placeholder::Never,
            2,
        )
        .await
        .unwrap()
        .id;
    ApiTokenStore::put(
        &db,
        &ApiToken {
            id: "t1".into(),
            name: "laptop".into(),
            owner_subject: stray.clone(),
            scope: Scope::Org("acme".into()),
            role: scarab_identity::Role::Member,
            expires_at: 10_000,
            created_by: stray.clone(),
            created_at: 2,
            last_used_at: None,
            use_count: 0,
            revoked_at: None,
        },
        "hash-1",
    )
    .await
    .unwrap();
    SessionStore::put(
        &db,
        &Session {
            id: "stray-session".into(),
            principal: Principal {
                subject: stray.clone(),
                handle: String::new(),
                display_name: None,
                roles: vec![],
            },
            expires_at: 10_000,
            csrf_hash: String::new(),
            created_at: 0,
            login_kind: scarab_identity::LoginKind::Unrecorded,
        },
    )
    .await
    .unwrap();

    // The stray Principal holds no passkey, so the OAuth round-trip alone
    // merges it (ADR-0079 §2).
    let outcome = db
        .link(&ada, &ident("1001", "ada-gh"), 3, &FoldedPasskey::Unproven)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        LinkOutcome::Merged {
            from: stray.clone(),
            from_handle: "github:ada-gh".into(),
            removed_passkeys: vec![],
        }
    );
    assert_eq!(
        db.by_hash("hash-1").await.unwrap().unwrap().owner_subject,
        ada,
        "the token follows the person"
    );
    assert_eq!(SessionStore::get(&db, "stray-session").await.unwrap(), None);
    let aliases: Vec<String> = db
        .aliases_of(&ada)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert_eq!(aliases, vec![stray.clone()]);
    // Linking it again is a no-op, not a second merge.
    assert_eq!(
        db.link(&ada, &ident("1001", "ada-gh"), 4, &FoldedPasskey::Unproven)
            .await
            .unwrap(),
        LinkOutcome::AlreadyLinked
    );
    // A second GitHub account on the same Principal is refused.
    assert!(matches!(
        db.link(&ada, &ident("3003", "ada-alt"), 5, &FoldedPasskey::Unproven)
            .await,
        Err(IdentityError::Conflict(_))
    ));
    tdb.cleanup().await;
}

/// ADR-0079 §2-3 (git-bug bbb5204): folding in a Principal that holds a
/// passkey needs proof from one of ITS passkeys. Without it, or with a
/// passkey of anyone else (the caller's own), nothing changes. With it, the
/// merge happens and the folded Principal's passkeys are deleted, never
/// moved to the caller.
#[tokio::test]
async fn a_merge_of_a_principal_with_passkeys_needs_one_of_its_passkeys() {
    use scarab_identity::{FoldedPasskey, LinkOutcome};
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let okta = ProviderIdentity {
        provider: "okta".into(),
        ..ident("00u-mallory", "mallory@example.com")
    };
    let mallory = db
        .sign_in(&okta, "okta:mallory@example.com", Placeholder::Never, 1)
        .await
        .unwrap()
        .id;
    let ada = db
        .sign_in(&ident("1001", "ada"), "github:ada", Placeholder::Never, 2)
        .await
        .unwrap()
        .id;
    for (id, owner, cred) in [("pk-m", &mallory, 1u8), ("pk-a", &ada, 2u8)] {
        sqlx::query(
            "INSERT INTO passkeys (id, principal_id, credential_id, public_key_cose, alg,
                 sign_count, label, created_at, created_session_hash)
             VALUES ($1, $2, $3, '\\x00', -7, 0, 'key', 1, 'h')",
        )
        .bind(id)
        .bind(owner)
        .bind(vec![cred])
        .execute(&tdb.pool)
        .await
        .unwrap();
    }
    let owners = || async {
        sqlx::query_as::<_, (String, String)>("SELECT id, principal_id FROM passkeys ORDER BY id")
            .fetch_all(&tdb.pool)
            .await
            .unwrap()
    };
    // Mallory, holding Ada's GitHub sign-in, links it.
    let gh_ada = ident("1001", "ada");
    for proof in [
        FoldedPasskey::Unproven,
        FoldedPasskey::Proven("pk-m".into()),
        FoldedPasskey::Proven("no-such".into()),
    ] {
        assert_eq!(
            db.link(&mallory, &gh_ada, 3, &proof).await.unwrap(),
            LinkOutcome::NeedsFoldedPasskey {
                from: ada.clone(),
                from_handle: "github:ada".into(),
            },
            "{proof:?}"
        );
    }
    assert_eq!(
        owners().await,
        vec![
            ("pk-a".to_string(), ada.clone()),
            ("pk-m".to_string(), mallory.clone())
        ],
        "nothing changed"
    );
    assert!(db.aliases_of(&mallory).await.unwrap().is_empty());

    // With a passkey of Ada's: merged, and Ada's passkey is deleted.
    assert_eq!(
        db.link(&mallory, &gh_ada, 4, &FoldedPasskey::Proven("pk-a".into()))
            .await
            .unwrap(),
        LinkOutcome::Merged {
            from: ada.clone(),
            from_handle: "github:ada".into(),
            removed_passkeys: vec!["pk-a".into()],
        }
    );
    assert_eq!(
        owners().await,
        vec![("pk-m".to_string(), mallory.clone())],
        "deleted, not moved"
    );
    tdb.cleanup().await;
}

/// git-bug 1a81c63: a passkey enrolled for the folded Principal after the
/// merge read its passkeys (a row lock does not hold back an INSERT) would
/// be deleted by the merge without the proof ADR-0079 §2 asks for. The
/// interleaving is built, not raced: a statement trigger on an UPDATE the
/// merge always makes before its DELETE inserts the passkey inside the
/// merge's transaction. The merge then changes nothing.
#[tokio::test]
async fn a_passkey_enrolled_during_a_merge_stops_it() {
    use scarab_identity::{FoldedPasskey, LinkOutcome};
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let okta = ProviderIdentity {
        provider: "okta".into(),
        ..ident("00u-mallory", "mallory@example.com")
    };
    let mallory = db
        .sign_in(&okta, "okta:mallory@example.com", Placeholder::Never, 1)
        .await
        .unwrap()
        .id;
    let ada = db
        .sign_in(&ident("1001", "ada"), "github:ada", Placeholder::Never, 2)
        .await
        .unwrap()
        .id;
    sqlx::query(&format!(
        "CREATE FUNCTION enroll_mid_merge() RETURNS trigger AS $$ BEGIN
           INSERT INTO passkeys (id, principal_id, credential_id, public_key_cose, alg,
               sign_count, label, created_at, created_session_hash)
           VALUES ('pk-late', '{ada}', '\\x09', '\\x00', -7, 0, 'late', 1, 'h')
           ON CONFLICT DO NOTHING;
           RETURN NULL;
         END $$ LANGUAGE plpgsql"
    ))
    .execute(&tdb.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER enroll_mid_merge AFTER UPDATE ON org_environments
         FOR EACH STATEMENT EXECUTE FUNCTION enroll_mid_merge()",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();

    assert_eq!(
        db.link(&mallory, &ident("1001", "ada"), 3, &FoldedPasskey::Unproven)
            .await
            .unwrap(),
        LinkOutcome::NeedsFoldedPasskey {
            from: ada.clone(),
            from_handle: "github:ada".into(),
        }
    );
    assert!(db.aliases_of(&mallory).await.unwrap().is_empty());
    tdb.cleanup().await;
}

/// `resolve_principal` follows merges to the live Principal — and a merge
/// chain that loops (a corrupt row; no code path writes one) is an error after
/// a bounded walk, never a hang on the request that typed the subject.
#[tokio::test]
async fn resolve_principal_follows_merges_and_refuses_a_merge_cycle() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let a = db
        .sign_in(&ident("1", "a"), "github:a", Placeholder::Never, 1)
        .await
        .unwrap()
        .id;
    let b = db
        .sign_in(&ident("2", "b"), "github:b", Placeholder::Never, 2)
        .await
        .unwrap()
        .id;

    sqlx::query("UPDATE principals SET merged_into = $2 WHERE id = $1")
        .bind(&a)
        .bind(&b)
        .execute(&tdb.pool)
        .await
        .unwrap();
    assert_eq!(
        scarab_identity::resolve_principal(&db, &a).await.unwrap(),
        Some(b.clone()),
        "a merged-away id resolves to its target"
    );

    sqlx::query("UPDATE principals SET merged_into = $2 WHERE id = $1")
        .bind(&b)
        .bind(&a)
        .execute(&tdb.pool)
        .await
        .unwrap();
    let err = scarab_identity::resolve_principal(&db, &a)
        .await
        .expect_err("a cycle must not resolve");
    assert!(err.to_string().contains("does not end"), "{err}");
}
