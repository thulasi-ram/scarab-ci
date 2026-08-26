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

// ---------------------------------------------------------------------------
// Master-key rotation (git-bug f37463a)
// ---------------------------------------------------------------------------

use scarab_secrets_postgres::{fingerprint, MasterKeySet, RewrapOutcome, SealedRow};

/// The AAD the adapter binds: `scope_text || 0x00 || key` (mirrored here for
/// raw-decrypt verification).
fn aad_of(scope_text: &str, key: &str) -> Vec<u8> {
    let mut aad = scope_text.as_bytes().to_vec();
    aad.push(0);
    aad.extend_from_slice(key.as_bytes());
    aad
}

async fn fetch_sealed(pool: &sqlx::PgPool, scope_text: &str, key: &str) -> SealedRow {
    let row = sqlx::query(
        "SELECT ciphertext, value_nonce, wrapped_key, key_nonce, key_id
         FROM secrets WHERE scope = $1 AND key = $2",
    )
    .bind(scope_text)
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap();
    SealedRow {
        ciphertext: row.get("ciphertext"),
        value_nonce: row.get("value_nonce"),
        wrapped_key: row.get("wrapped_key"),
        key_nonce: row.get("key_nonce"),
        key_id: row.get("key_id"),
    }
}

/// AES-256-GCM open with explicit AAD — raw verification, not via the adapter.
fn raw_decrypt(key: &[u8; 32], nonce: &[u8], ct: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    let cipher = Aes256Gcm::new_from_slice(key).unwrap();
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
        .ok()
}

/// Seal exactly the way the pre-AAD adapter did: no associated data.
fn legacy_seal(key: &[u8; 32], plaintext: &[u8]) -> (Vec<u8>, Vec<u8>) {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    use rand::RngCore;
    let cipher = Aes256Gcm::new_from_slice(key).unwrap();
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ct = cipher.encrypt(Nonce::from_slice(&nonce), plaintext).unwrap();
    (ct, nonce.to_vec())
}

/// Every new write stamps the active key's fingerprint and a wrapped_at.
#[tokio::test]
async fn new_write_stamps_the_active_fingerprint() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let k1 = [11u8; 32];
    let secrets = PostgresSecrets::with_master(tdb.pool.clone(), k1);
    secrets.migrate().await.unwrap();
    secrets
        .put(
            &repo_scope("app"),
            Secret {
                key: "T".into(),
                value: b"v".to_vec(),
            },
        )
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT key_id, (wrapped_at IS NOT NULL) AS stamped_at FROM secrets WHERE key = 'T'",
    )
    .fetch_one(&tdb.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<Option<String>, _>("key_id"), Some(fingerprint(&k1)));
    assert!(row.get::<bool, _>("stamped_at"));

    tdb.cleanup().await;
}

/// Rotation round-trip: a row written under K1 opens under [K2, K1]
/// (decrypt-only member), is restamped to K2 by rewrap-on-read WITHOUT the
/// value ciphertext moving, and afterwards opens under [K2] alone.
#[tokio::test]
async fn rotation_rewraps_on_read_and_restamps() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let (k1, k2) = ([21u8; 32], [22u8; 32]);
    let old = PostgresSecrets::with_master(tdb.pool.clone(), k1);
    old.migrate().await.unwrap();
    old.put(
        &repo_scope("app"),
        Secret {
            key: "R".into(),
            value: b"rotate-me".to_vec(),
        },
    )
    .await
    .unwrap();
    let before = fetch_sealed(&tdb.pool, "repo:acme/app", "R").await;

    let rotated =
        PostgresSecrets::with_keys(tdb.pool.clone(), MasterKeySet::new(vec![k2, k1]).unwrap());
    let got = rotated.get(&repo_scope("app"), "R").await.unwrap();
    assert_eq!(got.value, b"rotate-me".to_vec());

    let after = fetch_sealed(&tdb.pool, "repo:acme/app", "R").await;
    assert_eq!(after.key_id, Some(fingerprint(&k2)), "restamped to the active key");
    assert_ne!(after.wrapped_key, before.wrapped_key, "data key re-wrapped");
    assert_eq!(after.ciphertext, before.ciphertext, "value ciphertext never moves");
    assert_eq!(after.value_nonce, before.value_nonce, "value seal untouched");

    // The old key can now be dropped entirely.
    let only_new = PostgresSecrets::with_master(tdb.pool.clone(), k2);
    let got = only_new.get(&repo_scope("app"), "R").await.unwrap();
    assert_eq!(got.value, b"rotate-me".to_vec());

    tdb.cleanup().await;
}

/// A legacy (NULL key_id, empty-AAD) row is upgraded WHOLE on read: stamped,
/// and BOTH seals now AAD-bound — verified by raw decrypt, not the adapter.
#[tokio::test]
async fn legacy_empty_aad_row_is_upgraded_whole_on_read() {
    use rand::RngCore;

    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let master = [31u8; 32];
    let secrets = PostgresSecrets::with_master(tdb.pool.clone(), master);
    secrets.migrate().await.unwrap();

    let mut data_key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut data_key);
    let (ciphertext, value_nonce) = legacy_seal(&data_key, b"old-value");
    let (wrapped_key, key_nonce) = legacy_seal(&master, &data_key);
    sqlx::query(
        "INSERT INTO secrets (scope, key, ciphertext, value_nonce, wrapped_key, key_nonce)
         VALUES ('repo:acme/app', 'UP', $1, $2, $3, $4)",
    )
    .bind(&ciphertext)
    .bind(&value_nonce)
    .bind(&wrapped_key)
    .bind(&key_nonce)
    .execute(&tdb.pool)
    .await
    .unwrap();

    let got = secrets.get(&repo_scope("app"), "UP").await.unwrap();
    assert_eq!(got.value, b"old-value".to_vec());

    // Whole-row upgrade: stamped, and each seal opens ONLY AAD-bound now.
    let after = fetch_sealed(&tdb.pool, "repo:acme/app", "UP").await;
    assert_eq!(after.key_id, Some(fingerprint(&master)));
    let aad = aad_of("repo:acme/app", "UP");
    let dk = raw_decrypt(&master, &after.key_nonce, &after.wrapped_key, &aad)
        .expect("wrap must be AAD-bound after the upgrade");
    assert!(
        raw_decrypt(&master, &after.key_nonce, &after.wrapped_key, b"").is_none(),
        "empty-AAD arm must be dead on the upgraded wrap"
    );
    let dk: [u8; 32] = dk.try_into().unwrap();
    let value = raw_decrypt(&dk, &after.value_nonce, &after.ciphertext, &aad)
        .expect("value must be AAD-bound after the upgrade");
    assert_eq!(value, b"old-value".to_vec());
    assert!(
        raw_decrypt(&dk, &after.value_nonce, &after.ciphertext, b"").is_none(),
        "empty-AAD arm must be dead on the upgraded value"
    );

    tdb.cleanup().await;
}

/// A post-AAD, pre-rotation row (NULL key_id but AAD-bound — what put() wrote
/// between the AAD fix and key stamping) gets the stamp-only rewrap: the data
/// key moves to the active key, the value seal is NOT touched.
#[tokio::test]
async fn post_aad_null_row_gets_stamp_only_rewrap() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let (k1, k2) = ([41u8; 32], [42u8; 32]);
    let old = PostgresSecrets::with_master(tdb.pool.clone(), k1);
    old.migrate().await.unwrap();
    old.put(
        &repo_scope("app"),
        Secret {
            key: "N".into(),
            value: b"null-stamped".to_vec(),
        },
    )
    .await
    .unwrap();
    // Reproduce the pre-rotation shape: AAD-bound seals, no stamp.
    sqlx::query("UPDATE secrets SET key_id = NULL, wrapped_at = NULL WHERE key = 'N'")
        .execute(&tdb.pool)
        .await
        .unwrap();
    let before = fetch_sealed(&tdb.pool, "repo:acme/app", "N").await;

    let rotated =
        PostgresSecrets::with_keys(tdb.pool.clone(), MasterKeySet::new(vec![k2, k1]).unwrap());
    let got = rotated.get(&repo_scope("app"), "N").await.unwrap();
    assert_eq!(got.value, b"null-stamped".to_vec());

    let after = fetch_sealed(&tdb.pool, "repo:acme/app", "N").await;
    assert_eq!(after.key_id, Some(fingerprint(&k2)), "stamped to the active key");
    assert_ne!(after.wrapped_key, before.wrapped_key);
    assert_eq!(after.ciphertext, before.ciphertext, "value untouched on the AAD arm");
    assert_eq!(after.value_nonce, before.value_nonce, "value untouched on the AAD arm");

    tdb.cleanup().await;
}

/// The two stamped-row failures and the NULL-row failure are three
/// DISTINGUISHABLE errors, each naming fingerprints (never key material):
///  1. stamped under an unconfigured key → names the row's fp + configured fps;
///  2. stamped, right key, corrupted bytes → corruption/tamper (wrong key
///     ruled out because stamped rows are always AAD-bound);
///  3. legacy NULL row no key opens → "pre-rotation key absent OR corrupt"
///     (GCM cannot distinguish, and the message says so).
#[tokio::test]
async fn failure_taxonomy_is_distinguishable() {
    use rand::RngCore;

    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let (k1, k2) = ([51u8; 32], [52u8; 32]);
    let writer = PostgresSecrets::with_master(tdb.pool.clone(), k1);
    writer.migrate().await.unwrap();
    writer
        .put(
            &repo_scope("app"),
            Secret {
                key: "S".into(),
                value: b"v".to_vec(),
            },
        )
        .await
        .unwrap();

    // 1. The sealing key was dropped from the configured set.
    let without_k1 = PostgresSecrets::with_master(tdb.pool.clone(), k2);
    let err = without_k1.get(&repo_scope("app"), "S").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains(&fingerprint(&k1)), "names the row's key: {msg}");
    assert!(msg.contains(&fingerprint(&k2)), "names the configured set: {msg}");
    assert!(msg.contains("not configured"), "{msg}");

    // 2. Right key configured, but the stamped row's wrapped_key is corrupt
    // (XOR the first byte so the change is guaranteed, whatever it was).
    sqlx::query(
        "UPDATE secrets
         SET wrapped_key = set_byte(wrapped_key, 0, get_byte(wrapped_key, 0) # 255)
         WHERE key = 'S'",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();
    let err = writer.get(&repo_scope("app"), "S").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("corrupt") || msg.contains("tampered"), "{msg}");
    assert!(msg.contains(&fingerprint(&k1)), "names the stamped key: {msg}");
    assert!(!msg.contains("not configured"), "distinct from taxonomy 1: {msg}");

    // 3. A legacy NULL row sealed under a key nobody configures any more.
    let k3 = [53u8; 32];
    let mut data_key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut data_key);
    let (ct, vn) = legacy_seal(&data_key, b"lost");
    let (wk, kn) = legacy_seal(&k3, &data_key);
    sqlx::query(
        "INSERT INTO secrets (scope, key, ciphertext, value_nonce, wrapped_key, key_nonce)
         VALUES ('repo:acme/app', 'L', $1, $2, $3, $4)",
    )
    .bind(&ct)
    .bind(&vn)
    .bind(&wk)
    .bind(&kn)
    .execute(&tdb.pool)
    .await
    .unwrap();
    let err = writer.get(&repo_scope("app"), "L").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("pre-rotation master key is absent"), "{msg}");
    assert!(msg.contains("corrupt"), "GCM cannot distinguish — the error says so: {msg}");
    assert!(msg.contains(&fingerprint(&k1)), "names the configured set: {msg}");

    tdb.cleanup().await;
}

/// The rewrap's compare-and-swap: a rewrap holding a STALE snapshot (the
/// race's intermediate state — the table moved on after the snapshot was
/// read) matches nothing, reports LostRace, and never clobbers the newer row.
#[tokio::test]
async fn rewrap_cas_loses_to_a_concurrent_put() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let (k1, k2) = ([61u8; 32], [62u8; 32]);
    let old = PostgresSecrets::with_master(tdb.pool.clone(), k1);
    old.migrate().await.unwrap();
    old.put(
        &repo_scope("app"),
        Secret {
            key: "C".into(),
            value: b"first".to_vec(),
        },
    )
    .await
    .unwrap();

    // The reader's snapshot, taken before the concurrent writer runs.
    let stale = fetch_sealed(&tdb.pool, "repo:acme/app", "C").await;

    // A concurrent put wins the row (new value, new data key, active stamp).
    let rotated =
        PostgresSecrets::with_keys(tdb.pool.clone(), MasterKeySet::new(vec![k2, k1]).unwrap());
    rotated
        .put(
            &repo_scope("app"),
            Secret {
                key: "C".into(),
                value: b"second".to_vec(),
            },
        )
        .await
        .unwrap();
    let winner = fetch_sealed(&tdb.pool, "repo:acme/app", "C").await;

    // The stale rewrap must lose without touching the winner.
    let outcome = rotated
        .rewrap_row("repo:acme/app", "C", &stale)
        .await
        .unwrap();
    assert_eq!(outcome, RewrapOutcome::LostRace);
    let after = fetch_sealed(&tdb.pool, "repo:acme/app", "C").await;
    assert_eq!(after.wrapped_key, winner.wrapped_key, "winner not clobbered");
    assert_eq!(after.ciphertext, winner.ciphertext, "winner not clobbered");
    let got = rotated.get(&repo_scope("app"), "C").await.unwrap();
    assert_eq!(got.value, b"second".to_vec());

    tdb.cleanup().await;
}

