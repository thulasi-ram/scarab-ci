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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// One log stream, as the secret values that redact it are kept: a single
/// attempt of a step, or a sidecar or shared-service stream
/// ([`sidecar_stream_key`], [`service_stream_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFence {
    pub run: RunId,
    pub step: StepId,
    pub attempt: AttemptId,
}

impl LogFence {
    pub fn new(run: &RunId, step: &StepId, attempt: &AttemptId) -> Self {
        Self {
            run: run.clone(),
            step: step.clone(),
            attempt: attempt.clone(),
        }
    }

    /// The fence of `step`'s current attempt; `None` before it has one.
    pub fn of_step(step: &scarab_engine::StepRun) -> Option<Self> {
        step.current_attempt()
            .map(|a| Self::new(&step.run, &step.step, &a.id))
    }

    fn key(&self) -> StreamKey {
        stream_key(&self.run, &self.step, &self.attempt)
    }
}

/// The secret values of one log stream.
#[derive(Default)]
struct FenceValues {
    values: Mutex<Vec<Vec<u8>>>,
    /// Every value the stream's unit holds is registered: read back from the
    /// unit (`Executor::log_redactions`), or registered by the launch on this
    /// process.
    loaded: AtomicBool,
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
    /// Secret values to scrub from a stream's chunks before they are stored
    /// or streamed (ADR-0013, 0032), per stream: a value redacts only the
    /// stream it was registered for. Registered when secrets are injected
    /// into a step, and by a log tail from the unit it reads; removed once
    /// the tail has closed the stream and the unit has ended
    /// ([`forget_secrets`](Self::forget_secrets)).
    /// There is no server-wide value: every value Scarab registers belongs
    /// to one launch. A [`StreamRedactor`] holds its stream's entry and reads
    /// it at each push.
    secrets: Mutex<HashMap<StreamKey, Arc<FenceValues>>>,
}

impl LogService {
    pub fn new(store: Arc<dyn ObjectStore>, db: Arc<dyn Db>) -> Self {
        Self {
            store,
            db,
            live: Mutex::new(HashMap::new()),
            secrets: Mutex::new(HashMap::new()),
        }
    }

    /// The values entry of `fence`, created when missing.
    fn fence_values(&self, fence: &LogFence) -> Arc<FenceValues> {
        self.secrets
            .lock()
            .unwrap()
            .entry(fence.key())
            .or_default()
            .clone()
    }

    /// Register a secret value of `fence`'s unit so it is redacted from the
    /// stream's later chunks, and from no other stream. Empty values are
    /// ignored. Idempotent.
    pub fn register_secret(&self, fence: &LogFence, value: &[u8]) {
        if value.is_empty() {
            return;
        }
        let entry = self.fence_values(fence);
        let mut values = entry.values.lock().unwrap();
        if !values.iter().any(|v| v == value) {
            values.push(value.to_vec());
        }
    }

    /// Every value of `fence`'s unit is registered now (see
    /// [`values_loaded`](Self::values_loaded)).
    pub fn mark_values_loaded(&self, fence: &LogFence) {
        self.fence_values(fence)
            .loaded
            .store(true, Ordering::SeqCst);
    }

    /// Whether every value of `fence`'s unit is registered: read back from
    /// the unit, or registered by its launch on this process. A log tail
    /// stores nothing from a stream whose values are not loaded.
    pub fn values_loaded(&self, fence: &LogFence) -> bool {
        self.secrets
            .lock()
            .unwrap()
            .get(&fence.key())
            .is_some_and(|e| e.loaded.load(Ordering::SeqCst))
    }

    /// Drop `fence`'s values: its log tail has closed the stream and its
    /// unit has ended, so its failure cause is recorded. A redactor already
    /// made for it keeps them.
    pub fn forget_secrets(&self, fence: &LogFence) {
        self.secrets.lock().unwrap().remove(&fence.key());
    }

    /// Every value registered for `fence`.
    #[cfg(test)]
    pub(crate) fn registered_secrets(&self, fence: &LogFence) -> Vec<Vec<u8>> {
        self.secrets
            .lock()
            .unwrap()
            .get(&fence.key())
            .map(|e| e.values.lock().unwrap().clone())
            .unwrap_or_default()
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
    /// The caller knows the unit's handle, not its stream, so this uses the
    /// values of every stream registered now. A stream's values live until
    /// its tail has closed it and the driver pass after the unit ended, so
    /// the terminal poll of a unit still has them, and the set is bounded by
    /// the units that have not ended yet.
    ///
    /// Lossy by design on invalid UTF-8: a diagnostic is for reading.
    pub fn redact_text(&self, text: &str) -> String {
        let entries: Vec<Arc<FenceValues>> =
            self.secrets.lock().unwrap().values().cloned().collect();
        let mut values: Vec<Vec<u8>> = Vec::new();
        for entry in entries {
            for v in entry.values.lock().unwrap().iter() {
                if !values.contains(v) {
                    values.push(v.clone());
                }
            }
        }
        let all = FenceValues {
            values: Mutex::new(values),
            loaded: AtomicBool::new(true),
        };
        let out = redact_with(Arc::new(all), text.as_bytes());
        String::from_utf8_lossy(&out).into_owned()
    }

    /// A redactor for one read of `fence`'s log source, which can split a
    /// secret value across two chunks. It redacts the values registered for
    /// `fence` only. See [`StreamRedactor`].
    pub fn redactor(&self, fence: &LogFence) -> StreamRedactor {
        // A stream with no entry gets an empty one that is not kept: a
        // value registered later goes to the kept entry, which this
        // redactor does not see. A tail registers before it reads.
        let values = self
            .secrets
            .lock()
            .unwrap()
            .get(&fence.key())
            .cloned()
            .unwrap_or_default();
        StreamRedactor {
            secrets: values,
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
        let mut redactor = self.redactor(&LogFence::new(run, step, attempt));
        let mut redacted = redactor.push(chunk);
        redacted.extend(redactor.finish());
        let chunk = redacted;
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
    secrets: Arc<FenceValues>,
    held: Vec<u8>,
}

impl StreamRedactor {
    /// Redact the next chunk of the source; returns what can be emitted now.
    pub fn push(&mut self, src: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.held);
        buf.extend_from_slice(src);
        // A snapshot of the stream's values, so the scan does not hold the
        // lock a launch registering a value waits on.
        let values = self.secrets.values.lock().unwrap().clone();
        let (out, held_from) = redact_scan(&buf, &values, false);
        buf.drain(..held_from);
        self.held = buf;
        out
    }

    /// The source ended cleanly: redact and emit the held bytes.
    pub fn finish(self) -> Vec<u8> {
        let values = self.secrets.values.lock().unwrap().clone();
        redact_scan(&self.held, &values, true).0
    }
}

/// Redact all of `data` with `values`: one [`StreamRedactor`] given all of
/// it at once.
fn redact_with(values: Arc<FenceValues>, data: &[u8]) -> Vec<u8> {
    let mut redactor = StreamRedactor {
        secrets: values,
        held: Vec::new(),
    };
    let mut out = redactor.push(data);
    out.extend(redactor.finish());
    out
}

/// One left-to-right pass of [`StreamRedactor`] over `buf`. Returns the
/// output and the index where the held suffix starts (`buf.len()` when
/// nothing is held; always that when `at_end`).
fn redact_scan(buf: &[u8], values: &[Vec<u8>], at_end: bool) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(buf.len());
    let longest = values.iter().map(Vec::len).max().unwrap_or(0);
    let mut i = 0;
    while i < buf.len() {
        let rest = &buf[i..];
        // The rest of the buffer may be the start of a value whose end has
        // not been read yet: hold it, even when a shorter value matches here,
        // so the longest match wins as it would on the whole stream.
        if !at_end
            && rest.len() < longest
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
    use super::{sidecar_stream_key, FenceValues, LogFence, LogService, StreamRedactor};
    use scarab_engine::{AttemptId, Db, RunId, StepId};
    use scarab_testkit::{InMemoryDb, InMemoryObjectStore};
    use std::sync::{Arc, Mutex};

    fn redactor(values: &[&[u8]]) -> StreamRedactor {
        StreamRedactor {
            secrets: Arc::new(FenceValues {
                values: Mutex::new(values.iter().map(|v| v.to_vec()).collect()),
                ..Default::default()
            }),
            held: Vec::new(),
        }
    }

    fn service() -> LogService {
        LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            Arc::new(InMemoryDb::new()) as Arc<dyn Db>,
        )
    }

    fn fence(run: &str, attempt: &str) -> LogFence {
        LogFence::new(
            &RunId(run.into()),
            &StepId("build".into()),
            &AttemptId(attempt.into()),
        )
    }

    /// RT-R3-2 / R3-3: values are kept per stream. Registering the tokens of
    /// many other launches does not grow the set a stream's redactor scans
    /// (the cost per byte), and drops when each of them closes.
    #[test]
    fn values_of_other_streams_do_not_reach_a_stream() {
        let logs = service();
        let mine = fence("r1", "a1");
        logs.register_secret(&mine, b"my-token");
        for i in 0..1000 {
            let other = fence(&format!("other-{i}"), "a1");
            logs.register_secret(&other, format!("token-{i}").as_bytes());
        }
        assert_eq!(logs.registered_secrets(&mine), vec![b"my-token".to_vec()]);
        let r = logs.redactor(&mine);
        assert_eq!(r.secrets.values.lock().unwrap().len(), 1);

        // Another stream's value is not redacted here.
        let mut r = logs.redactor(&mine);
        let mut out = r.push(b"token-7 my-token\n");
        out.extend(r.finish());
        assert_eq!(out, b"token-7 ***\n".to_vec());

        // A closed stream's values are gone.
        for i in 0..1000 {
            logs.forget_secrets(&fence(&format!("other-{i}"), "a1"));
        }
        assert_eq!(logs.secrets.lock().unwrap().len(), 1);
        logs.forget_secrets(&mine);
        assert!(logs.registered_secrets(&mine).is_empty());
        assert!(!logs.values_loaded(&mine));
    }

    /// A stream's values are loaded only once marked, and the mark is per
    /// stream.
    #[test]
    fn values_loaded_is_per_stream() {
        let logs = service();
        let a = fence("r1", "a1");
        let b = fence("r1", "a2");
        logs.register_secret(&a, b"x");
        assert!(!logs.values_loaded(&a));
        logs.mark_values_loaded(&a);
        assert!(logs.values_loaded(&a));
        assert!(!logs.values_loaded(&b));
    }

    /// A diagnostic is redacted with the values of every open stream.
    #[test]
    fn redact_text_uses_every_open_stream() {
        let logs = service();
        logs.register_secret(&fence("r1", "a1"), b"one-secret");
        logs.register_secret(&fence("r2", "a1"), b"two-secret");
        assert_eq!(logs.redact_text("a one-secret b two-secret"), "a *** b ***");
        logs.forget_secrets(&fence("r1", "a1"));
        assert_eq!(logs.redact_text("one-secret"), "one-secret");
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
