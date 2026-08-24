//! Live S3/MinIO acceptance for the verbs the Depot's boot probe and pack
//! reads stand on (ADR-0067 part 1): `put`, `get_range`, `delete`.
//!
//! Every other test in this crate runs the `LocalFileSystem` backend, which
//! proves the adapter's logic but not the wire: ranged reads, delete
//! semantics and error mapping are exactly the things an S3-compatible server
//! can disagree about. CI stands up a real MinIO (`.github/workflows/ci.yml`)
//! and sets `SCARAB_TEST_REQUIRE_S3=1`, so this can never silently become a
//! skip there (the live-tier lesson: an env-gated test that `return`s on a
//! missing var can go green executing nothing). Locally, `just test` wires the
//! compose MinIO up; without it the test skips loudly.

use scarab_storage::{ObjectStore, StorageError};
use scarab_storage_s3::S3Storage;

/// The live store, or `None` when no test MinIO is configured (skip — unless
/// CI's `SCARAB_TEST_REQUIRE_S3=1` turns the skip into a panic).
fn live_store() -> Option<S3Storage> {
    let vars = [
        "SCARAB_TEST_S3_ENDPOINT",
        "SCARAB_TEST_S3_BUCKET",
        "SCARAB_TEST_S3_ACCESS_KEY",
        "SCARAB_TEST_S3_SECRET_KEY",
    ];
    let missing: Vec<&str> = vars
        .iter()
        .copied()
        .filter(|v| std::env::var(v).map(|s| s.is_empty()).unwrap_or(true))
        .collect();
    if !missing.is_empty() {
        if std::env::var("SCARAB_TEST_REQUIRE_S3").is_ok_and(|v| v == "1") {
            panic!(
                "live-S3 test skipped but SCARAB_TEST_REQUIRE_S3=1 — missing: {}",
                missing.join(", ")
            );
        }
        eprintln!(
            "SKIPPED (live-S3 test): set {} — `just test` wires the compose MinIO up",
            vars.join(", ")
        );
        return None;
    }
    let get = |k: &str| std::env::var(k).expect("checked above");
    Some(
        S3Storage::s3(
            get("SCARAB_TEST_S3_BUCKET"),
            &get("SCARAB_TEST_S3_ENDPOINT"),
            "us-east-1",
            &get("SCARAB_TEST_S3_ACCESS_KEY"),
            &get("SCARAB_TEST_S3_SECRET_KEY"),
        )
        .expect("live S3 store handle"),
    )
}

/// A key unique to this invocation, under the same `probe/` prefix the boot
/// probe uses — no run of this test can collide with a parallel one, and a
/// crashed run's residue can never be mistaken for content.
fn unique_key() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("probe/test-{}-{nanos}", std::process::id())
}

/// The boot probe's exact verb sequence, against the real wire: write, read a
/// RANGE back (both a slice and the whole object), delete, and prove the
/// delete stuck. `get_range` rather than `get` is deliberate — it is the verb
/// every durable pack-member miss resolves through (ADR-0067 part 9), and a
/// server that answers `GET` but not `Range` must fail HERE, not at the first
/// warm-missed read in production.
#[tokio::test]
async fn put_ranged_read_delete_round_trip_on_the_real_wire() {
    let Some(store) = live_store() else { return };
    let key = unique_key();
    let body: Vec<u8> = (0..=255u8).cycle().take(4096).collect();

    store.put(&key, body.clone()).await.expect("put");

    let slice = store.get_range(&key, 100, 56).await.expect("ranged read");
    assert_eq!(
        slice,
        &body[100..156],
        "a ranged read returns exactly the requested window"
    );
    let whole = store
        .get_range(&key, 0, body.len() as u64)
        .await
        .expect("full-length ranged read");
    assert_eq!(whole, body, "a full-length range is the whole object");

    store.delete(&key).await.expect("delete");
    assert!(
        matches!(store.get(&key).await, Err(StorageError::NotFound)),
        "after delete the key is NotFound — not an error, not stale bytes"
    );
}
