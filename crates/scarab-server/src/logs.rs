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

/// Map a **shared-service** instance `{run, take, name}` (ADR-0058) onto the log
/// pipeline's `(StepId, AttemptId)` stream key. A shared service is not a DAG
/// step, but ADR-0058 says its logs are the same best-effort tail (ADR-0013) as
/// step logs — so instead of a second pipeline it reuses this one under a stable
/// synthetic key: the `service:` prefix cannot collide with a real step id (step
/// ids are validated identifiers, no colon), and the Take maps to the attempt
/// slot so a Rerun's fresh instance keeps its own stream. Kept in one place so
/// the tail source and the read endpoint agree on the key.
pub fn service_stream_key(name: &str, take: i64) -> (StepId, AttemptId) {
    (
        StepId(format!("service:{name}")),
        AttemptId(format!("t{take}")),
    )
}

/// Map a **step sidecar** service (ADR-0058) — the `index`-th entry of a step's
/// authored `services:`, co-located in the step's Pod as container
/// `service-{index}` — onto the log pipeline's stream key. Like
/// [`service_stream_key`] this reuses the one pipeline via a synthetic step id
/// instead of a second channel: `{step}::service-{index}` cannot collide with a
/// real step id (validated identifiers carry no `:`), and the sidecar's logs are
/// stored under the step's REAL attempt id (a sidecar dies with + is re-created
/// alongside the step's Pod on every Attempt), so a per-attempt read scopes them
/// exactly like the step's own. Kept beside the step key so the tail source and
/// the read endpoint agree.
pub fn sidecar_stream_key(step: &StepId, index: usize) -> StepId {
    StepId(format!("{}::service-{index}", step.0))
}

/// Persists and streams step logs.
pub struct LogService {
    store: Arc<dyn ObjectStore>,
    db: Arc<dyn Db>,
    /// Live broadcast channels, one per in-flight stream.
    live: Mutex<HashMap<StreamKey, broadcast::Sender<Vec<u8>>>>,
    /// Secret values to scrub from every chunk before it is stored or streamed
    /// (ADR-0013, 0032). Registered when secrets are injected into a step,
    /// and by a log tail from the unit it reads. Shared with every
    /// [`StreamRedactor`], which reads it at each push.
    secrets: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl LogService {
    pub fn new(store: Arc<dyn ObjectStore>, db: Arc<dyn Db>) -> Self {
        Self {
            store,
            db,
            live: Mutex::new(HashMap::new()),
            secrets: Arc::new(Mutex::new(Vec::new())),
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

    /// Replace every registered secret value in a **text diagnostic** with
    /// `***` — the same scrub [`append`](Self::append) applies to log bytes,
    /// for the strings that never travel as log bytes.
    ///
    /// A failure cause or an infra condition quotes whatever the backend said,
    /// and a backend quotes registry URLs and auth-failure bodies. Those land
    /// in Postgres (`attempts.failure_detail`, the event log) and are served on
    /// the API, neither of which passes through the log pipeline — so without
    /// this they would be the one uncensored channel out of a step.
    ///
    /// Lossy by design on invalid UTF-8: a diagnostic is for reading.
    pub fn redact_text(&self, text: &str) -> String {
        String::from_utf8_lossy(&self.redact(text.as_bytes())).into_owned()
    }

    /// Replace every registered secret value in `data` with `***`: one
    /// [`StreamRedactor`] given all of `data` at once.
    fn redact(&self, data: &[u8]) -> Vec<u8> {
        let mut redactor = self.redactor();
        let mut out = redactor.push(data);
        out.extend(redactor.finish());
        out
    }

    /// A redactor for one read of a log source, which can split a secret
    /// value across two chunks. See [`StreamRedactor`].
    pub fn redactor(&self) -> StreamRedactor {
        StreamRedactor {
            secrets: self.secrets.clone(),
            held: Vec::new(),
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
        // Scrub secrets before the bytes touch the store OR the live stream, so
        // a value never reaches stored or streamed logs (ADR-0013, 0032).
        let chunk = self.redact(chunk);
        self.append_redacted(run, step, attempt, &chunk).await
    }

    /// [`append`](Self::append) for bytes a [`StreamRedactor`] already
    /// redacted: stored and streamed exactly as given. A log tail redacts
    /// its source as one stream, so a secret split across two chunks is
    /// still replaced.
    pub(crate) async fn append_redacted(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        chunk: &[u8],
    ) -> Result<LogChunkMeta, LogError> {
        // Next seq / cumulative uncompressed offset from the newest index row
        // (seqs are dense from 0, so it carries the whole stream's length).
        let (seq, byte_offset) = match self.db.last_log_chunk(run, step, attempt).await? {
            Some(last) => (last.seq + 1, last.byte_offset + last.len),
            None => (0, 0),
        };

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

    /// Whether any chunk of the stream is stored, by any replica.
    pub async fn has_output(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<bool, LogError> {
        Ok(self.db.last_log_chunk(run, step, attempt).await?.is_some())
    }

    /// A stream's chunk index, in `seq` order (durable, replica-agnostic:
    /// ADR-0051). Metadata only; the bodies stay in the object store.
    pub async fn chunks(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Vec<LogChunkMeta>, LogError> {
        Ok(self.db.log_chunks(run, step, attempt).await?)
    }

    /// The rows of [`Self::chunks`] with `seq >= from_seq`: what a live poll
    /// has not read yet.
    pub async fn chunks_from(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        from_seq: u64,
    ) -> Result<Vec<LogChunkMeta>, LogError> {
        Ok(self
            .db
            .log_chunks_from(run, step, attempt, from_seq)
            .await?)
    }

    /// Fetch and decompress one indexed chunk.
    pub async fn read_chunk(&self, meta: &LogChunkMeta) -> Result<Vec<u8>, LogError> {
        let blob = self.store.get(&meta.object_key).await?;
        gunzip(&blob).map_err(|e| LogError::Gzip(e.to_string()))
    }

    /// A stream's whole log in one buffer. For tests and small reads; the
    /// HTTP routes stream chunk by chunk ([`chunks`](Self::chunks) +
    /// [`read_chunk`](Self::read_chunk)).
    pub async fn read_all(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Vec<u8>, LogError> {
        let mut out = Vec::new();
        for c in self.chunks(run, step, attempt).await? {
            out.extend(self.read_chunk(&c).await?);
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

/// What a redacted secret value is replaced with.
const REDACTED: &[u8] = b"***";

/// Redacts one log source read as a stream of chunks, so a secret value
/// split across two chunks is still replaced.
///
/// Bytes are scanned left to right; at each position the longest registered
/// value that matches is replaced by `***`, otherwise one byte is copied.
/// [`push`](Self::push) holds back the longest suffix of what it has seen
/// that is a proper prefix of a registered value (at most the longest value
/// less one byte) until the next push shows whether it is a secret. A chunk
/// that does not end in the start of a value is emitted whole, so a live
/// tail is not delayed. The registered values are read at each push.
///
/// [`finish`](Self::finish) emits the held bytes, at the clean end of the
/// source only. A read that fails drops the redactor and its held bytes:
/// they may be the start of a secret, and the next read sees them again.
pub struct StreamRedactor {
    secrets: Arc<Mutex<Vec<Vec<u8>>>>,
    held: Vec<u8>,
}

impl StreamRedactor {
    /// Redact the next chunk of the source; returns what can be emitted now.
    pub fn push(&mut self, src: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.held);
        buf.extend_from_slice(src);
        let secrets = self.secrets.lock().unwrap();
        let (out, held_from) = redact_scan(&buf, &secrets, false);
        drop(secrets);
        buf.drain(..held_from);
        self.held = buf;
        out
    }

    /// The source ended cleanly: redact and emit the held bytes.
    pub fn finish(self) -> Vec<u8> {
        let secrets = self.secrets.lock().unwrap();
        redact_scan(&self.held, &secrets, true).0
    }
}

/// One left-to-right pass of [`StreamRedactor`] over `buf`. Returns the
/// output and the index where the held suffix starts (`buf.len()` when
/// nothing is held; always that when `at_end`).
fn redact_scan(buf: &[u8], values: &[Vec<u8>], at_end: bool) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        let rest = &buf[i..];
        // The rest of the buffer may be the start of a value whose end has
        // not been read yet: hold it, even when a shorter value matches here,
        // so the longest match wins as it would on the whole stream.
        if !at_end
            && values
                .iter()
                .any(|v| v.len() > rest.len() && v.starts_with(rest))
        {
            return (out, i);
        }
        match values
            .iter()
            .filter(|v| rest.starts_with(v))
            .map(Vec::len)
            .max()
        {
            Some(n) => {
                out.extend_from_slice(REDACTED);
                i += n;
            }
            None => {
                out.push(buf[i]);
                i += 1;
            }
        }
    }
    (out, buf.len())
}

#[cfg(test)]
mod tests {
    use super::{sidecar_stream_key, StreamRedactor};
    use scarab_engine::StepId;
    use std::sync::{Arc, Mutex};

    fn redactor(values: &[&[u8]]) -> StreamRedactor {
        StreamRedactor {
            secrets: Arc::new(Mutex::new(values.iter().map(|v| v.to_vec()).collect())),
            held: Vec::new(),
        }
    }

    /// Feed `chunks` through one redactor and return everything it emitted.
    fn run(values: &[&[u8]], chunks: &[&[u8]]) -> Vec<u8> {
        let mut r = redactor(values);
        let mut out = Vec::new();
        for c in chunks {
            out.extend(r.push(c));
        }
        out.extend(r.finish());
        out
    }

    /// A secret split across two chunks is replaced, as in one chunk.
    #[test]
    fn a_secret_split_across_chunks_is_redacted() {
        assert_eq!(
            run(&[b"token-value"], &[b"tok", b"en-value\n"]),
            b"***\n".to_vec()
        );
        // Split one byte at a time.
        let src = b"a token-value b";
        let chunks: Vec<&[u8]> = src.chunks(1).collect();
        assert_eq!(run(&[b"token-value"], &chunks), b"a *** b".to_vec());
    }

    /// Only a suffix that could start a secret is held; a line ending in
    /// anything else is emitted at once.
    #[test]
    fn only_the_start_of_a_secret_is_held() {
        let mut r = redactor(&[b"token-value"]);
        assert_eq!(r.push(b"line one\n"), b"line one\n".to_vec());
        assert_eq!(r.push(b"abc tok"), b"abc ".to_vec());
        // Not the secret after all: the held bytes are emitted.
        assert_eq!(r.push(b"io\n"), b"tokio\n".to_vec());
        assert_eq!(r.finish(), Vec::<u8>::new());
    }

    /// The longest value wins, also when a shorter one ends inside the held
    /// part.
    #[test]
    fn the_longest_value_wins_across_chunks() {
        assert_eq!(
            run(&[b"abc", b"abcdef"], &[b"x abc", b"def y"]),
            b"x *** y".to_vec()
        );
        assert_eq!(
            run(&[b"abc", b"abcdef"], &[b"x abc", b"dex"]),
            b"x ***dex".to_vec()
        );
    }

    /// At the end of the source the held bytes are emitted, redacted.
    #[test]
    fn finish_emits_the_held_bytes() {
        let mut r = redactor(&[b"token-value"]);
        assert_eq!(r.push(b"ends in tok"), b"ends in ".to_vec());
        assert_eq!(r.finish(), b"tok".to_vec());
    }

    #[test]
    fn sidecar_stream_key_namespaces_by_step_and_index() {
        // Nests under the step id and is distinct per index; the `::` guards it
        // from colliding with a real (colon-free) step id, so a sidecar's stream
        // never overwrites the step's own or a sibling sidecar's.
        assert_eq!(
            sidecar_stream_key(&StepId("test".into()), 0).0,
            "test::service-0"
        );
        assert_eq!(
            sidecar_stream_key(&StepId("test".into()), 1).0,
            "test::service-1"
        );
        assert_ne!(
            sidecar_stream_key(&StepId("build".into()), 0).0,
            sidecar_stream_key(&StepId("build".into()), 1).0,
        );
    }
}
