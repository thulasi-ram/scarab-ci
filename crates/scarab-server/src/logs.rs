//! Log pipeline (ADR-0013): step stdout/stderr → chunked, **compressed**
//! object-store blobs + a per-step byte-offset **index in Postgres** (never the
//! bodies) → live SSE tail + full replay.
//!
//! [`LogService`] orchestrates the two ports — `ObjectStore` (blobs) and `Db`
//! (offset index) — and an in-process broadcast for live tailing. It is infra
//! composition (both ports at once), so it lives in the server, not a pure
//! crate. The Pod-stdout *source* (the executor's k8s log tail) feeds
//! [`LogService::append`]; that wiring lands with the converged/dev-harness
//! slices.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use tokio::sync::broadcast;

use scarab_engine::{AttemptId, Db, LogChunkMeta, RunId, StepId};
use scarab_storage::ObjectStore;

/// Errors from the log pipeline.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("db error: {0}")]
    Db(#[from] scarab_engine::DbError),
    #[error("object store error: {0}")]
    Store(#[from] scarab_storage::StorageError),
    #[error("gzip error: {0}")]
    Gzip(String),
}

/// Identifies one log stream: a single attempt of a step.
type StreamKey = (String, String, String);

fn stream_key(run: &RunId, step: &StepId, attempt: &AttemptId) -> StreamKey {
    (run.0.clone(), step.0.clone(), attempt.0.clone())
}

fn object_key(run: &RunId, step: &StepId, attempt: &AttemptId, seq: u64) -> String {
    format!("logs/{}/{}/{}/{seq:08}.gz", run.0, step.0, attempt.0)
}

/// Persists and streams step logs.
pub struct LogService {
    store: Arc<dyn ObjectStore>,
    db: Arc<dyn Db>,
    /// Live broadcast channels, one per in-flight stream.
    live: Mutex<HashMap<StreamKey, broadcast::Sender<Vec<u8>>>>,
}

impl LogService {
    pub fn new(store: Arc<dyn ObjectStore>, db: Arc<dyn Db>) -> Self {
        Self {
            store,
            db,
            live: Mutex::new(HashMap::new()),
        }
    }

    fn sender(&self, key: &StreamKey) -> broadcast::Sender<Vec<u8>> {
        let mut live = self.live.lock().unwrap();
        live.entry(key.clone())
            .or_insert_with(|| broadcast::channel(1024).0)
            .clone()
    }

    /// Subscribe to live chunks for a stream (uncompressed bodies).
    pub fn subscribe(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> broadcast::Receiver<Vec<u8>> {
        self.sender(&stream_key(run, step, attempt)).subscribe()
    }

    /// Append one log chunk: compress → object store, index offsets → Postgres,
    /// broadcast the (uncompressed) body to any live tailer.
    pub async fn append(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        chunk: &[u8],
    ) -> Result<LogChunkMeta, LogError> {
        // Next seq / cumulative uncompressed offset from the existing index.
        let existing = self.db.log_chunks(run, step, attempt).await?;
        let seq = existing.len() as u64;
        let byte_offset = existing.iter().map(|c| c.len).sum();

        let key = object_key(run, step, attempt, seq);
        let compressed = gzip(chunk).map_err(|e| LogError::Gzip(e.to_string()))?;
        self.store.put(&key, compressed).await?;

        let meta = LogChunkMeta {
            seq,
            byte_offset,
            len: chunk.len() as u64,
            object_key: key,
        };
        self.db.append_log_chunk(run, step, attempt, &meta).await?;

        // Best-effort live fan-out (ignored if there are no subscribers).
        let _ = self
            .sender(&stream_key(run, step, attempt))
            .send(chunk.to_vec());

        Ok(meta)
    }

    /// Replay a stream's full log by reading every indexed chunk from the object
    /// store and decompressing in order (the post-completion path).
    pub async fn read_all(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Vec<u8>, LogError> {
        let chunks = self.db.log_chunks(run, step, attempt).await?;
        let mut out = Vec::new();
        for c in chunks {
            let blob = self.store.get(&c.object_key).await?;
            out.extend(gunzip(&blob).map_err(|e| LogError::Gzip(e.to_string()))?);
        }
        Ok(out)
    }
}

fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(data)?;
    e.finish()
}

fn gunzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut d = GzDecoder::new(data);
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}
