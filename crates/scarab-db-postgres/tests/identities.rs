//! PG identity store (ADR-0069) against *real* Postgres: a sign-in is pinned
//! by `(provider, external_id)`, a new identity always mints a NEW Principal,
//! a minted id is never minted twice, and two racing first sign-ins of one
//! identity converge on one Principal. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{IdentityStore, ProviderIdentity};

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
        .sign_in(&ident("1001", "alice"), "github:alice", 10)
        .await
        .unwrap();
    assert_eq!(alice, "github:alice");
    let rec = db.principal(&alice).await.unwrap().unwrap();
    assert_eq!(rec.claimed_at, Some(10));
    assert_eq!(rec.display_name.as_deref(), Some("alice (display)"));

    // A rename: same external id, new handle → same Principal, handle refreshed.
    let again = db
        .sign_in(&ident("1001", "alice-renamed"), "github:alice-renamed", 20)
        .await
        .unwrap();
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
        .sign_in(&ident("2002", "alice"), "github:alice", 30)
        .await
        .unwrap();
    assert_eq!(stranger, "github:alice~2");
    let third = db
        .sign_in(&ident("3003", "alice"), "github:alice", 40)
        .await
        .unwrap();
    assert_eq!(third, "github:alice~3");

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
            db.sign_in(&ident("4004", "bob"), "github:bob", 1)
                .await
                .unwrap()
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.push(t.await.unwrap());
    }
    ids.dedup();
    assert_eq!(
        ids,
        vec!["github:bob".to_string()],
        "one identity, one Principal"
    );
    // And the losers' rolled-back mints left no `~N` principals behind.
    assert_eq!(db.principal("github:bob~2").await.unwrap(), None);
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
        .pin(&ident("3003", "carol"), "github:carol", 5)
        .await
        .unwrap();
    assert_eq!(carol, "github:carol");
    assert_eq!(
        db.principal(&carol).await.unwrap().unwrap().claimed_at,
        None
    );
    // Re-importing the same account is the same Principal, still unclaimed.
    assert_eq!(
        db.pin(&ident("3003", "carol"), "github:carol", 6)
            .await
            .unwrap(),
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
    assert_eq!(db.sign_in(&signed, "github:carol", 7).await.unwrap(), carol);
    assert_eq!(
        db.principal(&carol).await.unwrap().unwrap().claimed_at,
        Some(7)
    );

    // A later import (no email) refreshes the handle only.
    db.pin(&ident("3003", "carol-renamed"), "github:carol-renamed", 8)
        .await
        .unwrap();
    let held = db.identities_of(&carol).await.unwrap();
    assert_eq!(held[0].handle, "carol-renamed");
    assert_eq!(held[0].email.as_deref(), Some("carol@example.com"));
    tdb.cleanup().await;
}
