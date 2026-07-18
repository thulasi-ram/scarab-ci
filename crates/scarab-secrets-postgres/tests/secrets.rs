//! Envelope-encrypted secrets acceptance (ADR-0014, 0032) against *real*
//! Postgres: store/fetch round-trips, ciphertext (not plaintext) is what's at
//! rest, a wrong scope misses, and a wrong master key cannot decrypt. Skips
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_secrets::{Secret, SecretError, SecretProvider, SecretScope};
use scarab_secrets_postgres::PostgresSecrets;
use sqlx::Row;

fn repo_scope(repo: &str) -> SecretScope {
    SecretScope::Repo {
        org: "acme".into(),
        repo: repo.into(),
    }
}

#[tokio::test]
async fn store_fetch_ciphertext_at_rest_and_scoping() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let master = [7u8; 32];
    let secrets = PostgresSecrets::with_master(tdb.pool.clone(), master);
    secrets.migrate().await.unwrap();

    let plaintext = b"s3cr3t-token".to_vec();
    secrets
        .put(
            &repo_scope("app"),
            Secret {
                key: "TOKEN".into(),
                value: plaintext.clone(),
            },
        )
        .await
        .unwrap();

    // Round-trips to the original value.
    let got = secrets.get(&repo_scope("app"), "TOKEN").await.unwrap();
    assert_eq!(got.value, plaintext);

    // What's on disk is ciphertext, not the plaintext.
    let row = sqlx::query("SELECT ciphertext, wrapped_key FROM secrets WHERE key = 'TOKEN'")
        .fetch_one(&tdb.pool)
        .await
        .unwrap();
    let ciphertext: Vec<u8> = row.get("ciphertext");
    assert_ne!(ciphertext, plaintext);
    assert!(
        !ciphertext
            .windows(plaintext.len())
            .any(|w| w == &plaintext[..]),
        "plaintext must not appear at rest"
    );

    // A different scope does not see the secret.
    assert!(matches!(
        secrets.get(&repo_scope("other"), "TOKEN").await,
        Err(SecretError::NotFound)
    ));

    // Listing is scoped.
    assert_eq!(
        secrets.list_scoped(&repo_scope("app")).await.unwrap(),
        vec!["TOKEN".to_string()]
    );
    assert!(secrets
        .list_scoped(&repo_scope("other"))
        .await
        .unwrap()
        .is_empty());

    tdb.cleanup().await;
}

/// A provider with the wrong master key cannot decrypt what another wrote —
/// the value is protected by the master key, not just the database.
#[tokio::test]
async fn wrong_master_key_cannot_decrypt() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let writer = PostgresSecrets::with_master(tdb.pool.clone(), [1u8; 32]);
    writer.migrate().await.unwrap();
    writer
        .put(
            &repo_scope("app"),
            Secret {
                key: "K".into(),
                value: b"v".to_vec(),
            },
        )
        .await
        .unwrap();

    let attacker = PostgresSecrets::with_master(tdb.pool.clone(), [2u8; 32]);
    assert!(matches!(
        attacker.get(&repo_scope("app"), "K").await,
        Err(SecretError::Backend(_))
    ));

    tdb.cleanup().await;
}
