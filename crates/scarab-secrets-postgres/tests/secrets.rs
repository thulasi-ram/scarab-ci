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

/// The sealed tuple is bound to its row: copying (ciphertext, value_nonce,
/// wrapped_key, key_nonce) onto a different (scope, key) must fail to decrypt
/// (git-bug 4e1e40d) — DB write access alone cannot re-scope a secret.
#[tokio::test]
async fn moved_ciphertext_fails_on_the_other_row() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let secrets = PostgresSecrets::with_master(tdb.pool.clone(), [3u8; 32]);
    secrets.migrate().await.unwrap();

    secrets
        .put(
            &repo_scope("app"),
            Secret {
                key: "DEPLOY_TOKEN".into(),
                value: b"prod-secret".to_vec(),
            },
        )
        .await
        .unwrap();
    secrets
        .put(
            &repo_scope("other"),
            Secret {
                key: "GREETING".into(),
                value: b"hello".to_vec(),
            },
        )
        .await
        .unwrap();

    // An actor with UPDATE on the table moves the sealed tuple across rows.
    sqlx::query(
        "UPDATE secrets AS dst
         SET ciphertext = src.ciphertext, value_nonce = src.value_nonce,
             wrapped_key = src.wrapped_key, key_nonce = src.key_nonce
         FROM secrets AS src
         WHERE src.key = 'DEPLOY_TOKEN' AND dst.key = 'GREETING'",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();

    // The AAD no longer matches the row identity: decryption fails.
    assert!(matches!(
        secrets.get(&repo_scope("other"), "GREETING").await,
        Err(SecretError::Backend(_))
    ));
    // The original row is untouched and still opens.
    let got = secrets.get(&repo_scope("app"), "DEPLOY_TOKEN").await.unwrap();
    assert_eq!(got.value, b"prod-secret".to_vec());

    tdb.cleanup().await;
}

/// A row written before AAD binding (sealed with no associated data) still
/// decrypts via the legacy fallback path.
#[tokio::test]
async fn legacy_no_aad_row_still_decrypts() {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    use rand::RngCore;

    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let master = [4u8; 32];
    let secrets = PostgresSecrets::with_master(tdb.pool.clone(), master);
    secrets.migrate().await.unwrap();

    // Seal exactly the way the pre-AAD adapter did: no associated data.
    let legacy_seal = |key: &[u8; 32], plaintext: &[u8]| {
        let cipher = Aes256Gcm::new_from_slice(key).unwrap();
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let ct = cipher.encrypt(Nonce::from_slice(&nonce), plaintext).unwrap();
        (ct, nonce.to_vec())
    };
    let mut data_key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut data_key);
    let (ciphertext, value_nonce) = legacy_seal(&data_key, b"old-value");
    let (wrapped_key, key_nonce) = legacy_seal(&master, &data_key);

    sqlx::query(
        "INSERT INTO secrets (scope, key, ciphertext, value_nonce, wrapped_key, key_nonce)
         VALUES ('repo:acme/app', 'OLD', $1, $2, $3, $4)",
    )
    .bind(&ciphertext)
    .bind(&value_nonce)
    .bind(&wrapped_key)
    .bind(&key_nonce)
    .execute(&tdb.pool)
    .await
    .unwrap();

    let got = secrets.get(&repo_scope("app"), "OLD").await.unwrap();
    assert_eq!(got.value, b"old-value".to_vec());

    tdb.cleanup().await;
}
