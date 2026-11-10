//! Migration 0093 against real Postgres (ADR-0080): every Principal gets a
//! random id, its readable id becomes its handle, current state is rewritten
//! from handle to id, history is left as it was, and the short-lived rows go.
//!
//! A database migrated up to just before 0093 is seeded with what an install
//! of 0.3.0 holds: Principals, one merged into another, a binding written
//! before 0052 that names nobody, two handles that differ only in case, an
//! API token, approver lists, a passkey, a session and history. Then 0093
//! runs, and the store reads it back through its own code. Skips cleanly
//! when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_identity::{
    is_principal_id, ApiTokenStore, IdentityError, IdentityStore, Placeholder, ProviderIdentity,
    RbacStore, Scope,
};
use sqlx::PgPool;

const VERSION: i64 = 93;

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

/// What a 0.3.0 install holds, as its schema stores it: every reference is
/// the readable id.
const SEED: &str = r#"
INSERT INTO principals (id, display_name, created_at, claimed_at) VALUES
    ('github:alice', 'Alice', 1, 1),
    ('ada',          'Ada',   2, 2),
    ('github:Bob',   NULL,    3, 3),
    ('github:bob',   NULL,    4, 4),
    ('github:old',   NULL,    5, 5);
UPDATE principals SET merged_into = 'github:alice' WHERE id = 'github:old';
INSERT INTO principal_identities
    (provider, external_id, principal_id, handle, linked_at)
VALUES ('github', '1', 'github:alice', 'alice', 1),
       ('default', '2', 'ada', 'ada', 2),
       ('github', '3', 'github:Bob', 'Bob', 3),
       ('github', '4', 'github:bob', 'bob', 4);

INSERT INTO rbac_bindings (subject, org, project, role, origin) VALUES
    ('github:alice', 'acme', '',    'admin',  'native'),
    ('ada',          'acme', 'web', 'member', 'native'),
    ('legacy-carol', 'acme', '',    'member', 'native');

INSERT INTO api_tokens
    (id, token_hash, name, owner_subject, scope_org, role, expires_at, created_by, created_at)
VALUES ('t1', 'hash-1', 'laptop', 'github:alice', 'acme', 'member', 10000, 'github:alice', 1);

INSERT INTO org_environments (org, name, created_at, updated_at, updated_by, approvers)
VALUES ('acme', 'prod', 1, 1, 'github:alice', '{ada,github:alice}');
INSERT INTO environment_project_overlays
    (org, environment, project, approvers, updated_at, updated_by)
VALUES ('acme', 'prod', 'web', '{github:alice}', 1, 'github:alice');

INSERT INTO passkey_users (principal_id, user_handle) VALUES ('github:alice', '\x01');
INSERT INTO passkeys (id, principal_id, credential_id, public_key_cose, alg,
                      sign_count, label, created_at, created_session_hash)
VALUES ('pk-1', 'github:alice', '\x01', '\x00', -7, 0, 'key', 1, 'h');

INSERT INTO sessions (id, principal, csrf, expires_at)
VALUES (repeat('a', 64), '{"subject":"github:alice","display_name":null,"roles":[]}', '', 99999);
INSERT INTO webauthn_challenges
    (id, principal_id, session_hash, purpose, nonce, created_at, expires_at)
VALUES ('c1', 'github:alice', 'h', 'window', '\x00', 1, 99999);

INSERT INTO events (run_id, version, at, payload)
VALUES ('run-1', 1, 1, '{"GateApproved":{"step":"approve","by":"github:alice"}}');
INSERT INTO org_security_events (id, org, at, kind, subject, caller)
VALUES ('e1', 'acme', 1, 'step_up.confirmed', 'github:alice', 'session:cookie');
"#;

async fn id_of(db: &PostgresDb, handle: &str) -> String {
    let found = db.by_handle(handle).await.unwrap();
    assert_eq!(found.len(), 1, "{handle}: {found:?}");
    assert_eq!(found[0].handle, handle);
    found[0].id.clone()
}

fn ident(provider: &str, external_id: &str, handle: &str) -> ProviderIdentity {
    ProviderIdentity {
        provider: provider.into(),
        external_id: external_id.into(),
        handle: handle.into(),
        email: None,
        email_verified: false,
        display_name: None,
    }
}

#[tokio::test]
async fn current_state_moves_to_ids_and_history_keeps_its_handles() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    sqlx::raw_sql(SEED)
        .execute(&pool)
        .await
        .expect("seed 0.3.0");
    apply(&pool, |v| v >= VERSION).await;
    let db = PostgresDb::with_pool(pool.clone());

    // Every Principal has a random id; its readable id is now its handle.
    let alice = id_of(&db, "github:alice").await;
    let ada = id_of(&db, "ada").await;
    let old = id_of(&db, "github:old").await;
    for id in [&alice, &ada, &old] {
        assert!(is_principal_id(id), "{id}");
    }
    assert_ne!(alice, ada);
    let rec = db.principal(&alice).await.unwrap().unwrap();
    assert_eq!(rec.display_name.as_deref(), Some("Alice"));
    assert_eq!(rec.claimed_at, Some(1));
    assert_eq!(
        db.principal(&old).await.unwrap().unwrap().merged_into,
        Some(alice.clone()),
        "a merge points at the target's id"
    );
    assert_eq!(db.pinned("github", "1").await.unwrap(), Some(alice.clone()));

    // Bindings: rewritten to ids. The pre-0052 subject nobody held got an
    // unclaimed placeholder, and its binding names that.
    let carol = id_of(&db, "legacy-carol").await;
    let carol_rec = db.principal(&carol).await.unwrap().unwrap();
    assert_eq!(carol_rec.claimed_at, None);
    assert!(db.identities_of(&carol).await.unwrap().is_empty());
    let mut subjects: Vec<String> = RbacStore::bindings(&db, "acme")
        .await
        .unwrap()
        .into_iter()
        .map(|(b, _)| b.subject)
        .collect();
    subjects.sort();
    let mut want = vec![alice.clone(), ada.clone(), carol.clone()];
    want.sort();
    assert_eq!(subjects, want);
    assert_eq!(
        db.role_of(&alice, &Scope::Org("acme".into()))
            .await
            .unwrap(),
        Some(scarab_identity::Role::Admin)
    );

    // The token follows its owner's id; who minted it is history.
    let token = db.by_hash("hash-1").await.unwrap().unwrap();
    assert_eq!(token.owner_subject, alice);
    assert_eq!(token.created_by, "github:alice");

    // Approver lists keep their order.
    let approvers: Vec<String> =
        sqlx::query_scalar("SELECT approvers FROM org_environments WHERE name = 'prod'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(approvers, vec![ada.clone(), alice.clone()]);
    let overlay: Vec<String> =
        sqlx::query_scalar("SELECT approvers FROM environment_project_overlays")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(overlay, vec![alice.clone()]);

    // Passkeys follow the id.
    let held: Vec<String> = sqlx::query_scalar("SELECT principal_id FROM passkeys")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(held, vec![alice.clone()]);
    let user: String = sqlx::query_scalar("SELECT principal_id FROM passkey_users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(user, alice);

    // Everyone signs in again.
    for table in ["sessions", "webauthn_challenges"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }

    // History is not rewritten.
    let by: String =
        sqlx::query_scalar("SELECT payload->'GateApproved'->>'by' FROM events WHERE seq = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(by, "github:alice");
    let subject: String = sqlx::query_scalar("SELECT subject FROM org_security_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(subject, "github:alice");
    let updated_by: String =
        sqlx::query_scalar("SELECT updated_by FROM org_environments WHERE name = 'prod'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(updated_by, "github:alice");

    // The read path takes either form.
    for r in ["github:alice", alice.as_str()] {
        assert_eq!(
            scarab_identity::lookup_principal(&db, r)
                .await
                .unwrap()
                .map(|p| p.id),
            Some(alice.clone()),
            "{r}"
        );
    }
    // A write resolves a handle (or an id) to the live id, following merges.
    assert_eq!(
        scarab_identity::resolve_principal(&db, "github:old")
            .await
            .unwrap(),
        Some(alice.clone())
    );
    assert_eq!(
        scarab_identity::resolve_principal(&db, "GITHUB:ALICE")
            .await
            .unwrap(),
        Some(alice.clone()),
        "a typed handle matches ignoring case when one does"
    );
    tdb.cleanup().await;
}

#[tokio::test]
async fn case_only_duplicates_are_kept_reported_and_never_minted_again() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    sqlx::raw_sql(SEED)
        .execute(&pool)
        .await
        .expect("seed 0.3.0");
    apply(&pool, |v| v >= VERSION).await;
    let db = PostgresDb::with_pool(pool.clone());

    // Both kept, each its own Principal, and reported oldest first.
    let upper = id_of(&db, "github:Bob").await;
    let lower = id_of(&db, "github:bob").await;
    assert_ne!(upper, lower);
    assert_eq!(
        db.case_duplicate_handles().await.unwrap(),
        vec![vec!["github:Bob".to_string(), "github:bob".to_string()]]
    );
    // Typed exactly, each resolves to itself; typed any other way, the
    // write is refused and names both.
    assert_eq!(
        scarab_identity::resolve_principal(&db, "github:bob")
            .await
            .unwrap(),
        Some(lower.clone())
    );
    match scarab_identity::resolve_principal(&db, "GitHub:BOB").await {
        Err(IdentityError::Conflict(why)) => {
            assert!(
                why.contains("`github:Bob`") && why.contains("`github:bob`"),
                "{why}"
            )
        }
        other => panic!("expected a conflict, got {other:?}"),
    }

    // A new sign-in whose handle equals one of them ignoring case takes the
    // next suffix.
    let third = db
        .sign_in(
            &ident("github", "5", "BOB"),
            "github:BOB",
            Placeholder::Never,
            10,
        )
        .await
        .unwrap();
    assert_eq!(third.handle, "github:BOB~2");
    // So does one that differs only in case from a single existing handle.
    let alice_again = db
        .sign_in(
            &ident("github", "6", "Alice"),
            "github:Alice",
            Placeholder::Never,
            11,
        )
        .await
        .unwrap();
    assert_eq!(alice_again.handle, "github:Alice~2");
    tdb.cleanup().await;
}

#[tokio::test]
async fn only_an_unqualified_sign_in_of_that_exact_string_adopts_a_placeholder() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    sqlx::raw_sql(SEED)
        .execute(&pool)
        .await
        .expect("seed 0.3.0");
    apply(&pool, |v| v >= VERSION).await;
    let db = PostgresDb::with_pool(pool.clone());
    let carol = id_of(&db, "legacy-carol").await;

    // A provider that may not adopt (a qualified one) mints around it, as it
    // would around any taken handle.
    let other = db
        .sign_in(
            &ident("okta", "x", "legacy-carol"),
            "legacy-carol",
            Placeholder::Never,
            5,
        )
        .await
        .unwrap();
    assert_ne!(other.id, carol);
    assert_eq!(other.handle, "legacy-carol~2");

    // The unqualified provider's sign-in that produces exactly that string
    // adopts it, and with it the grant written before 0052.
    let adopted = db
        .sign_in(
            &ident("default", "77", "legacy-carol"),
            "legacy-carol",
            Placeholder::AdoptExact,
            6,
        )
        .await
        .unwrap();
    assert_eq!(adopted.id, carol);
    assert_eq!(adopted.claimed_at, Some(6));
    assert_eq!(
        db.role_of(&carol, &Scope::Org("acme".into()))
            .await
            .unwrap(),
        Some(scarab_identity::Role::Member)
    );
    // Adopted once: the next one mints around it.
    let late = db
        .sign_in(
            &ident("default", "78", "legacy-carol"),
            "legacy-carol",
            Placeholder::AdoptExact,
            7,
        )
        .await
        .unwrap();
    assert_ne!(late.id, carol);
    tdb.cleanup().await;
}

/// The other half of the rewrite: a merge after the upgrade moves bindings,
/// approver entries and tokens by id.
#[tokio::test]
async fn a_merge_after_the_upgrade_moves_everything_by_id() {
    use scarab_identity::{FoldedPasskey, LinkOutcome};
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    sqlx::raw_sql(SEED)
        .execute(&pool)
        .await
        .expect("seed 0.3.0");
    apply(&pool, |v| v >= VERSION).await;
    let db = PostgresDb::with_pool(pool.clone());
    let ada = id_of(&db, "ada").await;
    let alice = id_of(&db, "github:alice").await;
    // The passkey `github:alice` held would stop a merge without proof
    // (ADR-0079); remove it, as an Owner's reset would.
    sqlx::query("DELETE FROM passkeys")
        .execute(&pool)
        .await
        .unwrap();

    // Ada links Alice's GitHub sign-in, Alice's only one: Alice is folded in.
    let outcome = db
        .link(
            &ada,
            &ident("github", "1", "alice"),
            9,
            &FoldedPasskey::Unproven,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        LinkOutcome::Merged {
            from: alice.clone(),
            from_handle: "github:alice".into(),
            removed_passkeys: vec![],
        }
    );
    assert_eq!(
        db.role_of(&ada, &Scope::Org("acme".into())).await.unwrap(),
        Some(scarab_identity::Role::Admin)
    );
    let approvers: Vec<String> =
        sqlx::query_scalar("SELECT approvers FROM org_environments WHERE name = 'prod'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(approvers, vec![ada.clone()], "folded, not listed twice");
    assert_eq!(
        ApiTokenStore::by_hash(&db, "hash-1")
            .await
            .unwrap()
            .unwrap()
            .owner_subject,
        ada
    );
    tdb.cleanup().await;
}
