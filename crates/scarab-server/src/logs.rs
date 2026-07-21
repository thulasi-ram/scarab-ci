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
    /// Secret values to scrub from every chunk before it is stored or streamed
    /// (ADR-0013, 0032). Registered when secrets are injected into a step.
    secrets: Mutex<Vec<Vec<u8>>>,
}

impl LogService {
    pub fn new(store: Arc<dyn ObjectStore>, db: Arc<dyn Db>) -> Self {
        Self {
            store,
            db,
            live: Mutex::new(HashMap::new()),
            secrets: Mutex::new(Vec::new()),
        }
    }

    /// Register a secret value so it is redacted from all subsequent log chunks.
    /// Empty values are ignored. Idempotent.
    pub fn register_secret(&self, value: &[u8]) {
        if value.is_empty() {
            return;
        }
        let mut secrets = self.secrets.lock().unwrap();
        if !secrets.iter().any(|v| v == value) {
            secrets.push(value.to_vec());
        }
    }

    /// Replace every registered secret value in `data` with `***`.
    fn redact(&self, data: &[u8]) -> Vec<u8> {
        let secrets = self.secrets.lock().unwrap();
        let mut out = data.to_vec();
        for value in secrets.iter() {
            out = replace_bytes(&out, value, b"***");
        }
        out
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
        // Scrub secrets before the bytes touch the store OR the live stream, so
        // a value never reaches stored or streamed logs (ADR-0013, 0032).
        let chunk = self.redact(chunk);

        // Next seq / cumulative uncompressed offset from the existing index.
        let existing = self.db.log_chunks(run, step, attempt).await?;
        let seq = existing.len() as u64;
        let byte_offset = existing.iter().map(|c| c.len).sum();

        let key = object_key(run, step, attempt, seq);
        let compressed = gzip(&chunk).map_err(|e| LogError::Gzip(e.to_string()))?;
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
            .send(chunk.clone());

        Ok(meta)
    }

    /// Read a stream's chunks from `from_seq` onward (durable index + store) —
    /// the replica-agnostic live-tail path (ADR-0051): ANY replica serves new
    /// chunks by re-reading the index the tailing replica writes. Returns the
    /// concatenated bodies and the next `from_seq`.
    pub async fn read_from(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        from_seq: u64,
    ) -> Result<(Vec<u8>, u64), LogError> {
        let chunks = self.db.log_chunks(run, step, attempt).await?;
        let mut out = Vec::new();
        let mut next = from_seq;
        for c in chunks.into_iter().filter(|c| c.seq >= from_seq) {
            let blob = self.store.get(&c.object_key).await?;
            out.extend(gunzip(&blob).map_err(|e| LogError::Gzip(e.to_string()))?);
            next = c.seq + 1;
        }
        Ok((out, next))
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

/// Replace every occurrence of `needle` in `haystack` with `repl`.
fn replace_bytes(haystack: &[u8], needle: &[u8], repl: &[u8]) -> Vec<u8> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if i + needle.len() <= haystack.len() && &haystack[i..i + needle.len()] == needle {
            out.extend_from_slice(repl);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}
