//! Log-pipeline acceptance (ADR-0013): full replay after completion, live tail
//! during the run, and NO log bodies in Postgres. Hermetic: an in-memory object
//! store (the true external, mocked at the boundary) + InMemoryDb index.

use std::sync::Arc;

use scarab_engine::{AttemptId, Db, RunId, StepId};
use scarab_server::LogService;
use scarab_testkit::{InMemoryDb, InMemoryObjectStore};

fn stream() -> (RunId, StepId, AttemptId) {
    (
        RunId("r".into()),
        StepId("build".into()),
        AttemptId("a1".into()),
    )
}

/// Appended chunks round-trip via read_all; bodies live compressed in the object
/// store while Postgres holds only offset index rows (no text).
#[tokio::test]
async fn full_replay_after_completion_keeps_bodies_out_of_postgres() {
    let db = Arc::new(InMemoryDb::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = LogService::new(store.clone(), db.clone());
    let (run, step, attempt) = stream();

    logs.append(&run, &step, &attempt, b"hello ").await.unwrap();
    logs.append(&run, &step, &attempt, b"world\n").await.unwrap();

    // Full replay reconstructs the exact byte stream.
    let all = logs.read_all(&run, &step, &attempt).await.unwrap();
    assert_eq!(all, b"hello world\n");

    // Bodies are compressed blobs in the object store...
    assert_eq!(store.len(), 2, "one compressed blob per chunk");

    // ...and Postgres holds only the offset index — object keys + offsets, no text.
    let idx = db.log_chunks(&run, &step, &attempt).await.unwrap();
    assert_eq!(idx.len(), 2);
    assert_eq!((idx[0].seq, idx[0].byte_offset, idx[0].len), (0, 0, 6));
    assert_eq!((idx[1].seq, idx[1].byte_offset, idx[1].len), (1, 6, 6));
    assert!(idx[0].object_key.ends_with(".gz"));
    assert!(!idx[0].object_key.contains("hello"));
}

/// A subscriber receives chunks live as they are appended (the during-run tail).
#[tokio::test]
async fn live_tail_receives_chunks_as_they_arrive() {
    let db = Arc::new(InMemoryDb::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = LogService::new(store, db);
    let (run, step, attempt) = stream();

    let mut rx = logs.subscribe(&run, &step, &attempt);
    logs.append(&run, &step, &attempt, b"line1\n").await.unwrap();
    logs.append(&run, &step, &attempt, b"line2\n").await.unwrap();

    assert_eq!(rx.recv().await.unwrap(), b"line1\n");
    assert_eq!(rx.recv().await.unwrap(), b"line2\n");
}
