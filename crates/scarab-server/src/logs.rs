//! Log pipeline (ADR-0013): step stdout/stderr → chunked, **compressed**
//! object-store blobs + a per-stream byte-offset **index in Postgres** (never
//! the bodies) → live SSE tail + full replay.
//!
//! [`LogService`] composes the two ports — `ObjectStore` (blobs) and `Db`
//! (index, stream state, value ledger) — so it lives in the server, not a
//! pure crate.
//!
//! **Writes** are fenced (git-bug 511a769): every row is appended through
//! [`Db::append_log_row`] under the writer's lease for that stream (the
//! store derives its resource from the stream id), and an output
//! row commits only while the ledger generation its bytes were redacted
//! with is the unit's current one. A body's object key is its content
//! address, so no PUT rewrites a committed body.
//!
//! **Redaction** (git-bug 2d78464): a unit's secret values are sealed in its
//! **value ledger** in Postgres before any of them is written where the unit
//! can read them ([`LogService::record_launch_values`]). Every replica reads
//! the ledger, all of it or nothing ([`LogService::load_values`]); a unit
//! with no ledger, or a purged one, is not ledger-complete and its source is
//! never read. The process keeps a cache of loaded ledgers, evicted after
//! [`VALUES_TTL`]: the ledger is the source.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use futures::stream::{self, BoxStream, StreamExt};

use scarab_engine::{
    AttemptId, Db, DbError, LogAppendError, LogBody, LogChunkMeta, LogLease, LogRowKind,
    LogStreamId, LogStreamState, LogWriter, MountedValue, NewLogRow, NotStoredReason, RunId,
    StepId, Timestamp,
};
use scarab_secrets::Sealer;
use scarab_storage::ObjectStore;

/// Errors from the log pipeline's reads.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("db error: {0}")]
    Db(#[from] scarab_engine::DbError),
    #[error("object store error: {0}")]
    Store(#[from] scarab_storage::StorageError),
    #[error("gzip error: {0}")]
    Gzip(String),
}

/// Why a unit's values could not be loaded.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LedgerError {
    /// The unit is not ledger-complete, for a reason every replica finds:
    /// it has no `log_units` row, or its ledger was purged. Its log is not
    /// stored.
    #[error("the unit is not ledger-complete ({})", .0.as_str())]
    NotLedgerComplete(NotStoredReason),
    /// This replica could not load it (a seal it cannot open, a missing
    /// generation, a database error). Never a reason to close a stream.
    #[error("the unit's values could not be loaded here: {0}")]
    Local(String),
}

/// How long a loaded value set is kept in the process cache.
pub const VALUES_TTL: Duration = Duration::from_secs(600);

/// The text a failure cause is replaced with when the unit it is about has
/// no ledger this replica can load.
pub const CAUSE_NOT_STORED: &str =
    "the cause was not stored: it may quote secret values that could not be checked";

/// The secret values of one unit, as its ledger holds them at generation
/// `gen`: the union of every generation `1..=gen`. Built only from a loaded
/// ledger (there is no empty default).
#[derive(Debug)]
pub struct ValueSet {
    gen: i64,
    entries: Vec<MountedValue>,
    values: Vec<Vec<u8>>,
    generations: Vec<LedgerGeneration>,
}

/// One generation of a unit's ledger as loaded: the values one launch
/// mounted, and when its minted forge tokens were revoked (so they are not
/// revoked again).
#[derive(Debug, Clone)]
pub struct LedgerGeneration {
    pub gen: i64,
    pub revoked_at: Option<i64>,
    pub entries: Vec<MountedValue>,
}

impl LedgerGeneration {
    /// The minted forge tokens in this generation, once each.
    pub fn minted_forge_tokens(&self) -> Vec<&MountedValue> {
        minted_once(&self.entries)
    }
}

fn minted_once(entries: &[MountedValue]) -> Vec<&MountedValue> {
    let mut out: Vec<&MountedValue> = Vec::new();
    for e in entries {
        if e.kind == scarab_engine::MountedValueKind::ForgeToken
            && e.minted == Some(true)
            && !out.iter().any(|o| o.value == e.value)
        {
            out.push(e);
        }
    }
    out
}

impl ValueSet {
    /// A set of one generation `gen` holding `entries`.
    #[cfg(test)]
    fn new(gen: i64, entries: Vec<MountedValue>) -> Self {
        Self::from_generations(
            gen,
            vec![LedgerGeneration {
                gen,
                revoked_at: None,
                entries,
            }],
        )
    }

    fn from_generations(gen: i64, generations: Vec<LedgerGeneration>) -> Self {
        let entries: Vec<MountedValue> = generations
            .iter()
            .flat_map(|g| g.entries.iter().cloned())
            .collect();
        let mut values: Vec<Vec<u8>> = Vec::new();
        for e in &entries {
            if !e.value.is_empty() && !values.contains(&e.value) {
                values.push(e.value.clone());
            }
        }
        Self {
            gen,
            entries,
            values,
            generations,
        }
    }

    /// Each generation, oldest first.
    pub fn generations(&self) -> &[LedgerGeneration] {
        &self.generations
    }

    /// The oldest generation that holds `value`, if any.
    pub fn generation_holding(&self, value: &[u8]) -> Option<i64> {
        self.generations
            .iter()
            .find(|g| g.entries.iter().any(|e| e.value == value))
            .map(|g| g.gen)
    }

    /// The ledger generation this set is.
    pub fn gen(&self) -> i64 {
        self.gen
    }

    /// Every value to redact.
    pub fn values(&self) -> &[Vec<u8>] {
        &self.values
    }

    /// Every entry, with its kind (forge tokens carry their repository and
    /// whether they were minted).
    pub fn entries(&self) -> &[MountedValue] {
        &self.entries
    }

    /// The minted forge tokens in the ledger, once each: what is revoked.
    pub fn minted_forge_tokens(&self) -> Vec<&MountedValue> {
        minted_once(&self.entries)
    }
}

/// A unit's key in the process cache.
type StreamKey = (String, String, String);

fn unit_key(unit: &LogStreamId) -> StreamKey {
    (
        unit.run.0.clone(),
        unit.step.0.clone(),
        unit.attempt.0.clone(),
    )
}

/// The AEAD associated data of generation `gen` of `unit`'s ledger: a seal
/// opens only under its own unit and generation.
pub fn ledger_aad(unit: &LogStreamId, gen: i64) -> Vec<u8> {
    format!(
        "scarab-log-values/v1:{}:{}:{}:{gen}",
        unit.run.0, unit.step.0, unit.attempt.0
    )
    .into_bytes()
}

/// A body's object key: its content address, so a PUT to a key always
/// carries the same bytes and a committed body is never rewritten.
fn object_key(stream: &LogStreamId, sha256: &[u8]) -> String {
    let hex: String = sha256.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "logs/{}/{}/{}/{hex}.gz",
        stream.run.0, stream.step.0, stream.attempt.0
    )
}

/// SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(data).to_vec()
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

pub use scarab_engine::tail_lease_resource;

/// The tail lease a 0.6.x server took for the same stream; a reader of a
/// stream with rows from 0.6.x checks it too.
pub fn legacy_tail_lease_resource(run: &RunId, step: &StepId, attempt: &AttemptId) -> String {
    match step.0.rsplit_once("::service-") {
        Some((owner, index)) => format!("tail:{}:{owner}:sidecar-{index}:{}", run.0, attempt.0),
        None => format!("tail:{}:{}:{}", run.0, step.0, attempt.0),
    }
}

/// How many stored value-ledger generations are sealed under a master key
/// other than `keys`' active one (git-bug 2d78464). They cannot be
/// rewrapped (the AAD binds unit and generation) and live at most a day;
/// key rotation adds them to the rows under a non-active key, so an old
/// key is not removed while a ledger still needs it.
pub async fn ledger_rows_under_nonactive_key(
    db: &dyn Db,
    keys: &scarab_secrets_postgres::KeySealer,
) -> Result<u64, DbError> {
    db.count_log_values_not_sealed_with(&keys.active_seal_prefix())
        .await
}

/// A loaded value set and when it was loaded.
struct Cached {
    set: Arc<ValueSet>,
    at: Instant,
}

/// Persists and streams step logs.
pub struct LogService {
    store: Arc<dyn ObjectStore>,
    db: Arc<dyn Db>,
    /// Seals and opens ledger generations: the deployment's master keys.
    sealer: Arc<dyn Sealer>,
    /// Loaded value sets by unit, evicted after [`VALUES_TTL`].
    cache: Mutex<HashMap<StreamKey, Cached>>,
    /// The lease owner of [`append`](Self::append), the test writer.
    owner: String,
}

impl LogService {
    /// A log service whose ledger seals under a fresh random key: what it
    /// seals opens only in this process. A server sets the deployment's
    /// sealer ([`with_sealer`](Self::with_sealer)).
    pub fn new(store: Arc<dyn ObjectStore>, db: Arc<dyn Db>) -> Self {
        Self {
            store,
            db,
            sealer: Arc::new(scarab_secrets_postgres::KeySealer::ephemeral()),
            cache: Mutex::new(HashMap::new()),
            owner: format!("logsvc-{}", uuid::Uuid::new_v4()),
        }
    }

    /// Seal and open ledger generations with `sealer` (the master keys):
    /// every replica must use the same keys, or a replica that cannot open
    /// a seal never reads that unit's log (a local error, never a close).
    pub fn with_sealer(mut self, sealer: Arc<dyn Sealer>) -> Self {
        self.sealer = sealer;
        self
    }

    pub fn db(&self) -> &Arc<dyn Db> {
        &self.db
    }

    // -- value ledger ------------------------------------------------------

    /// The unit's value set: from the cache, else loaded from its ledger.
    pub async fn load_values(&self, unit: &LogStreamId) -> Result<Arc<ValueSet>, LedgerError> {
        {
            let mut cache = self.cache.lock().unwrap();
            cache.retain(|_, c| c.at.elapsed() < VALUES_TTL);
            if let Some(c) = cache.get(&unit_key(unit)) {
                return Ok(c.set.clone());
            }
        }
        self.reload_values(unit).await
    }

    /// Load the unit's value set from its ledger, all of it or nothing:
    /// every generation `1..=gen` present and opened here. A unit with no
    /// row, or a purged one, is not ledger-complete (every replica finds
    /// the same); anything else that fails is local to this replica.
    pub async fn reload_values(&self, unit: &LogStreamId) -> Result<Arc<ValueSet>, LedgerError> {
        let rows = self
            .db
            .log_unit_ledger(unit)
            .await
            .map_err(|e| LedgerError::Local(e.to_string()))?;
        let Some(rows) = rows else {
            return Err(LedgerError::NotLedgerComplete(NotStoredReason::NoLedger));
        };
        if rows.unit.purged {
            self.cache.lock().unwrap().remove(&unit_key(unit));
            return Err(LedgerError::NotLedgerComplete(
                NotStoredReason::LedgerPurged,
            ));
        }
        let gen = rows.unit.gen;
        let gens: Vec<i64> = rows.generations.iter().map(|g| g.gen).collect();
        if gens != (1..=gen).collect::<Vec<_>>() {
            return Err(LedgerError::Local(format!(
                "the ledger of {}/{}/{} is at generation {gen} and holds {gens:?}",
                unit.run.0, unit.step.0, unit.attempt.0
            )));
        }
        let mut generations = Vec::new();
        for g in &rows.generations {
            let plain = self
                .sealer
                .open(&g.sealed, &ledger_aad(unit, g.gen))
                .map_err(|e| LedgerError::Local(format!("opening generation {}: {e}", g.gen)))?;
            let entries: Vec<MountedValue> = serde_json::from_slice(&plain)
                .map_err(|e| LedgerError::Local(format!("generation {}: {e}", g.gen)))?;
            generations.push(LedgerGeneration {
                gen: g.gen,
                revoked_at: g.revoked_at,
                entries,
            });
        }
        let set = Arc::new(ValueSet::from_generations(gen, generations));
        self.cache.lock().unwrap().insert(
            unit_key(unit),
            Cached {
                set: set.clone(),
                at: Instant::now(),
            },
        );
        Ok(set)
    }

    /// Record the values a launch of `unit` mounts, before any of them is
    /// written where the unit can read them (I7): upsert the unit's row
    /// (with `handle`), and when `values` adds anything to the ledger's
    /// union, seal `values` as the next generation. Returns the set the
    /// unit has after it. A purged unit or a ledger this replica cannot
    /// load fails the launch.
    pub async fn record_launch_values(
        &self,
        unit: &LogStreamId,
        handle: Option<&str>,
        values: Vec<MountedValue>,
        now: Timestamp,
    ) -> Result<Arc<ValueSet>, LedgerError> {
        for _ in 0..4 {
            let current = match self.reload_values(unit).await {
                Ok(set) => Some(set),
                Err(LedgerError::NotLedgerComplete(NotStoredReason::NoLedger)) => None,
                Err(e) => return Err(e),
            };
            let gen = current.as_ref().map_or(0, |c| c.gen);
            let adds = values.iter().any(|v| {
                !current
                    .as_ref()
                    .is_some_and(|c| c.values.contains(&v.value))
            });
            let sealed = if adds {
                let json =
                    serde_json::to_vec(&values).map_err(|e| LedgerError::Local(e.to_string()))?;
                Some(
                    self.sealer
                        .seal(&json, &ledger_aad(unit, gen + 1))
                        .map_err(|e| LedgerError::Local(format!("sealing: {e}")))?,
                )
            } else {
                None
            };
            match self
                .db
                .write_log_unit(unit, handle, gen, sealed.as_deref(), now)
                .await
            {
                Ok(_) => return self.reload_values(unit).await,
                // Another launch wrote a generation since the load: again.
                Err(DbError::Conflict) => continue,
                Err(e) => return Err(LedgerError::Local(e.to_string())),
            }
        }
        Err(LedgerError::Local(
            "the ledger kept changing under this launch".into(),
        ))
    }

    /// Redact a failure cause or infra message about the unit behind
    /// `handle` with that unit's ledger (I10), read at its current
    /// generation, never from the cache: a peer may have recorded a token
    /// since this replica loaded the unit. A unit with no ledger this
    /// replica can load gets [`CAUSE_NOT_STORED`] in place of the text.
    /// Lossy on invalid UTF-8: a diagnostic is for reading.
    pub async fn redact_text_for_handle(&self, handle: &str, text: &str) -> String {
        let unit = match self.db.log_unit_by_handle(handle).await {
            Ok(Some(unit)) => unit,
            _ => return CAUSE_NOT_STORED.to_string(),
        };
        match self.reload_values(&unit).await {
            Ok(set) => redact_text(&set, text),
            Err(_) => CAUSE_NOT_STORED.to_string(),
        }
    }

    /// Purge `unit`'s ledger if every one of its `stream_count` streams is
    /// terminal (a stream that has stored nothing yet is not), it has
    /// settled, and every minted token in it was revoked (or it holds none:
    /// `values`, when it is the ledger's current generation). The purge
    /// happens only at the generation read here, so one a late launch
    /// writes meanwhile is kept and revoked later. Best-effort: the purge
    /// sweep covers what this misses.
    pub async fn purge_if_done(&self, unit: &LogStreamId, values: Option<&ValueSet>) {
        let Ok(Some(rows)) = self.db.log_unit_ledger(unit).await else {
            return;
        };
        let revoked = rows.unit.revoke_attempted_at.is_some()
            || values
                .is_some_and(|v| v.gen() == rows.unit.gen && v.minted_forge_tokens().is_empty());
        if rows.unit.purged || rows.unit.settled_at.is_none() || !revoked {
            return;
        }
        if !matches!(self.db.log_unit_streams_all_terminal(unit).await, Ok(true)) {
            return;
        }
        match self.db.purge_log_unit(unit, rows.unit.gen).await {
            Ok(true) => {
                self.cache.lock().unwrap().remove(&unit_key(unit));
            }
            Ok(false) => {
                tracing::debug!("log ledger: a generation was written since the purge was decided; the sweep revokes and purges it");
            }
            Err(e) => {
                tracing::debug!(error = %e, "log ledger: purge after the last stream closed failed; the sweep retries");
            }
        }
    }

    /// Drop `unit` from the process cache.
    pub fn forget_values(&self, unit: &LogStreamId) {
        self.cache.lock().unwrap().remove(&unit_key(unit));
    }

    /// A redactor for one read of a part of `values`' unit, with the
    /// suffix rule on at the part's first byte.
    pub fn redactor(&self, values: Arc<ValueSet>) -> StreamRedactor {
        StreamRedactor::new(values)
    }

    // -- fenced writes -----------------------------------------------------

    /// Store `bytes` (already redacted with generation `values_gen`) as one
    /// output row of `stream`, fenced on `writer`: compress → object store
    /// under its content address, then the index row. `then` sets the
    /// stream's state in the same transaction.
    pub async fn append_output(
        &self,
        stream: &LogStreamId,
        bytes: &[u8],
        values_gen: i64,
        writer: &LogWriter,
        then: Option<LogStreamState>,
        now: Timestamp,
    ) -> Result<LogChunkMeta, LogAppendError> {
        let digest = sha256(bytes);
        let key = object_key(stream, &digest);
        let compressed =
            gzip(bytes).map_err(|e| LogAppendError::Db(DbError::Other(format!("gzip: {e}"))))?;
        self.store
            .put(&key, compressed)
            .await
            .map_err(|e| LogAppendError::Db(DbError::Other(format!("object store: {e}"))))?;
        let mut row = NewLogRow::output(
            LogBody {
                object_key: key,
                len: bytes.len() as u64,
                sha256: digest,
            },
            values_gen,
        );
        row.then = then;
        self.db.append_log_row(stream, &row, writer, now).await
    }

    /// Append a marker row to `stream`, fenced on `writer`.
    pub async fn append_marker(
        &self,
        stream: &LogStreamId,
        kind: LogRowKind,
        reason: Option<NotStoredReason>,
        writer: &LogWriter,
        then: Option<LogStreamState>,
        now: Timestamp,
    ) -> Result<LogChunkMeta, LogAppendError> {
        let mut row = NewLogRow::marker(kind, reason);
        row.then = then;
        self.db.append_log_row(stream, &row, writer, now).await
    }

    /// Append one chunk the way a tail does, for tests and tools that feed
    /// the pipeline directly: under the stream's append lease (not its tail
    /// lease), taken for the one write and let go after it, after
    /// giving the stream's unit a ledger (with no values) if it has none,
    /// redacted with the unit's current values. Not a production path.
    pub async fn append(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        chunk: &[u8],
    ) -> Result<LogChunkMeta, LogError> {
        let stream = LogStreamId::new(run, step, attempt);
        let unit = stream.unit();
        let other = |e: String| LogError::Db(DbError::Other(e));
        let values = match self.load_values(&unit).await {
            Ok(v) => v,
            Err(LedgerError::NotLedgerComplete(NotStoredReason::NoLedger)) => self
                .record_launch_values(&unit, None, Vec::new(), Timestamp(0))
                .await
                .map_err(|e| other(e.to_string()))?,
            Err(e) => return Err(other(e.to_string())),
        };
        // Its own lease, not the tail lease: a reader of the log takes a
        // live tail lease to mean the log is still being stored.
        let resource = LogLease::Append.resource(&stream);
        let lease = self.db.lease(&resource, &self.owner, 45_000).await?;
        let writer = LogWriter::holding(
            LogLease::Append,
            &lease
                .held_by(&resource, &self.owner)
                .ok_or(LogError::Db(DbError::NotLeader))?,
        );
        let mut redactor = StreamRedactor::new(values.clone());
        let mut out = redactor.push(chunk);
        out.extend(redactor.finish_confirmed());
        let appended = self
            .append_output(&stream, &out, values.gen, &writer, None, Timestamp(0))
            .await
            .map_err(|e| other(e.to_string()));
        // A one-shot writer: let the lease go (owner and term stay, so a
        // concurrent call of this service still appends), so another
        // writer of the stream (a test's `seed_log_row`) can take it.
        let _ = self.db.release_lease(&resource, &self.owner).await;
        appended
    }

    // -- reads -------------------------------------------------------------

    /// Whether any output row of the stream is stored, by any replica.
    pub async fn has_output(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<bool, LogError> {
        Ok(self
            .db
            .log_chunks(run, step, attempt)
            .await?
            .iter()
            .any(|c| c.is_output()))
    }

    /// A stream's index, output and marker rows, in `seq` order (durable,
    /// replica-agnostic: ADR-0051). Metadata only; the bodies stay in the
    /// object store.
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

    /// Fetch and decompress one output row's body. A marker has none.
    pub async fn read_chunk(&self, meta: &LogChunkMeta) -> Result<Vec<u8>, LogError> {
        let Some(key) = &meta.object_key else {
            return Ok(Vec::new());
        };
        let blob = self.store.get(key).await?;
        gunzip(&blob).map_err(|e| LogError::Gzip(e.to_string()))
    }

    /// Fetch and decompress the bodies of `chunks`, in the order given, with
    /// up to [`PREFETCH_CHUNKS`] fetches in flight. A marker row yields an
    /// empty body. At most [`PREFETCH_CHUNKS`] bodies are held ahead of the
    /// reader.
    pub fn read_chunks(
        &self,
        chunks: Vec<LogChunkMeta>,
    ) -> BoxStream<'static, Result<Vec<u8>, LogError>> {
        let store = self.store.clone();
        stream::iter(chunks)
            .map(move |c| {
                let store = store.clone();
                async move {
                    let Some(key) = &c.object_key else {
                        return Ok(Vec::new());
                    };
                    let blob = store.get(key).await?;
                    gunzip(&blob).map_err(|e| LogError::Gzip(e.to_string()))
                }
            })
            .buffered(PREFETCH_CHUNKS)
            .boxed()
    }

    /// A stream's whole log in one buffer, a marker row written as the raw
    /// download writes it (`\n[scarab] <text>\n`). For tests and small
    /// reads; the HTTP routes stream it.
    pub async fn read_all(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Vec<u8>, LogError> {
        let rows = self.chunks(run, step, attempt).await?;
        let mut out = Vec::new();
        let mut bodies = self.read_chunks(rows.clone());
        for row in rows {
            let body = bodies.next().await.unwrap_or(Ok(Vec::new()))?;
            if row.is_output() {
                out.extend(body);
            } else {
                out.extend(marker_line(&row));
            }
        }
        Ok(out)
    }

    /// The stored output bytes only, as `?markers=none` returns them.
    pub async fn read_output(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Vec<u8>, LogError> {
        let rows: Vec<_> = self
            .chunks(run, step, attempt)
            .await?
            .into_iter()
            .filter(|r| r.is_output())
            .collect();
        let mut out = Vec::new();
        let mut bodies = self.read_chunks(rows);
        while let Some(body) = bodies.next().await {
            out.extend(body?);
        }
        Ok(out)
    }
}

/// A marker row as a raw download writes it: `\n[scarab] <text>\n`.
pub fn marker_line(row: &LogChunkMeta) -> Vec<u8> {
    format!(
        "\n[scarab] {}\n",
        scarab_engine::marker_text(row.kind, row.reason)
    )
    .into_bytes()
}

/// Chunk bodies a reader of a stored log fetches ahead of what it has used
/// ([`LogService::read_chunks`]).
pub const PREFETCH_CHUNKS: usize = 16;

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

/// The shortest leading proper suffix of a value that is redacted at the
/// start of a part (residual R-1: a shorter one is stored).
pub const MIN_SUFFIX: usize = 8;

/// Redact a whole `text` with `values`. The text is known whole, so its
/// end is not a cut: a trailing start of a value is kept as it is.
pub fn redact_text(values: &Arc<ValueSet>, text: &str) -> String {
    let out = redact_scan(text.as_bytes(), values.values(), Scan::Whole).0;
    String::from_utf8_lossy(&out).into_owned()
}

/// Where a [`redact_scan`] stops.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scan {
    /// More bytes may follow: hold a trailing start of a value.
    Open,
    /// A confirmed end of a source: a trailing proper prefix of a value is
    /// `***` (the prefix rule).
    ConfirmedEnd,
    /// A whole text: nothing follows and nothing was cut.
    Whole,
}

/// Redacts one read of a log source as a stream of chunks, so a secret value
/// split across two chunks is still replaced, with the values of one ledger
/// generation (a new generation needs a new redactor).
///
/// Bytes are scanned left to right; at each position the longest value that
/// matches is replaced by `***`, otherwise one byte is copied.
/// [`push`](Self::push) holds back the longest suffix of what it has seen
/// that is a proper prefix of a value (at most the longest value less one
/// byte) until the next push shows whether it is a secret. A chunk that does
/// not end in the start of a value is emitted whole, so a live tail is not
/// delayed.
///
/// Edges (I1): [`finish_confirmed`](Self::finish_confirmed), at a confirmed
/// end of the source, writes a held proper prefix of a value as `***`. At
/// the first byte of a part (the first part of a stream, or one after a
/// `source_restarted` row) it writes the longest leading proper suffix of a
/// value of at least [`MIN_SUFFIX`] bytes as `***`: a read can start inside
/// a value when the kubelet rotated the log file. Until the bytes held at
/// the start show whether a longer value matches there, nothing is emitted,
/// so what is stored does not depend on how the source is chunked. A read
/// that fails drops the redactor and its held bytes: they may be the start
/// of a secret, and the next read sees them again.
pub struct StreamRedactor {
    values: Arc<ValueSet>,
    held: Vec<u8>,
    /// The suffix rule still applies to the first byte.
    suffix_pending: bool,
}

impl StreamRedactor {
    pub fn new(values: Arc<ValueSet>) -> Self {
        Self {
            values,
            held: Vec::new(),
            suffix_pending: true,
        }
    }

    /// The generation of the values this redactor uses.
    pub fn gen(&self) -> i64 {
        self.values.gen
    }

    /// The values this redactor uses.
    pub fn values(&self) -> &Arc<ValueSet> {
        &self.values
    }

    /// Redact the next chunk of the source; returns what can be emitted now.
    pub fn push(&mut self, src: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.held);
        buf.extend_from_slice(src);
        let mut out = Vec::new();
        if self.suffix_pending {
            match leading_suffix(&buf, self.values.values(), false) {
                SuffixAt::Undecided => {
                    self.held = buf;
                    return out;
                }
                SuffixAt::None => {}
                SuffixAt::Len(n) => {
                    out.extend_from_slice(REDACTED);
                    buf.drain(..n);
                }
            }
            self.suffix_pending = false;
        }
        let (scanned, held_from) = redact_scan(&buf, self.values.values(), Scan::Open);
        out.extend(scanned);
        buf.drain(..held_from);
        self.held = buf;
        out
    }

    /// The source's end is confirmed: emit the held bytes, a held proper
    /// prefix of a value as `***`.
    pub fn finish_confirmed(mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = std::mem::take(&mut self.held);
        if self.suffix_pending {
            if let SuffixAt::Len(n) = leading_suffix(&buf, self.values.values(), true) {
                out.extend_from_slice(REDACTED);
                buf.drain(..n);
            }
        }
        out.extend(redact_scan(&buf, self.values.values(), Scan::ConfirmedEnd).0);
        out
    }
}

/// What the suffix rule finds at the start of a part.
enum SuffixAt {
    /// More bytes could still make a longer suffix or a whole value match.
    Undecided,
    None,
    /// The first `n` bytes are a proper suffix of a value (`n >=
    /// MIN_SUFFIX`), and no whole value at the start is longer.
    Len(usize),
}

/// The suffix rule at the start of `buf`. Before a confirmed end (`at_end`
/// false) it decides only once no suffix and no whole value longer than
/// `buf` can still match there, so every chunking of a source decides as
/// the whole source in one chunk does (2599a7b: deciding on a suffix while
/// a longer whole value could still match stored that value's tail).
fn leading_suffix(buf: &[u8], values: &[Vec<u8>], at_end: bool) -> SuffixAt {
    if !at_end
        && values
            .iter()
            .any(|v| v.len() > buf.len() && v.starts_with(buf))
    {
        return SuffixAt::Undecided;
    }
    let mut best: Option<usize> = None;
    for v in values {
        for n in MIN_SUFFIX..v.len() {
            let suffix = &v[v.len() - n..];
            if buf.starts_with(suffix) {
                best = Some(best.map_or(n, |b| b.max(n)));
            } else if !at_end && suffix.len() > buf.len() && suffix.starts_with(buf) {
                return SuffixAt::Undecided;
            }
        }
    }
    let Some(n) = best else {
        return SuffixAt::None;
    };
    // A whole value at the start that is longer wins, as the longest
    // match does everywhere.
    if values.iter().any(|v| v.len() > n && buf.starts_with(v)) {
        return SuffixAt::None;
    }
    SuffixAt::Len(n)
}

/// One left-to-right pass over `buf`. Returns the output and the index where
/// the held suffix starts (`buf.len()` when nothing is held; always that
/// unless the scan is [`Scan::Open`]).
fn redact_scan(buf: &[u8], values: &[Vec<u8>], scan: Scan) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(buf.len());
    let longest = values.iter().map(Vec::len).max().unwrap_or(0);
    let mut i = 0;
    while i < buf.len() {
        let rest = &buf[i..];
        // The rest of the buffer may be the start of a value whose end has
        // not been read yet: hold it, even when a shorter value matches here,
        // so the longest match wins as it would on the whole stream. At a
        // confirmed end it is redacted instead.
        if scan != Scan::Whole
            && rest.len() < longest
            && values
                .iter()
                .any(|v| v.len() > rest.len() && v.starts_with(rest))
        {
            if scan == Scan::ConfirmedEnd {
                out.extend_from_slice(REDACTED);
                return (out, buf.len());
            }
            return (out, i);
        }
        match values
            .iter()
            .filter(|v| !v.is_empty() && rest.starts_with(v))
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
    use super::*;
    use proptest::prelude::*;
    use proptest::sample::Index;
    use scarab_engine::{MountedValueKind, StepId};

    fn set(values: &[&[u8]]) -> Arc<ValueSet> {
        Arc::new(ValueSet::new(
            1,
            values
                .iter()
                .map(|v| MountedValue {
                    kind: MountedValueKind::Secret,
                    value: v.to_vec(),
                    repo: None,
                    minted: None,
                })
                .collect(),
        ))
    }

    /// Feed `chunks` through one redactor and return everything it emitted
    /// up to a confirmed end.
    fn run(values: &[&[u8]], chunks: &[&[u8]]) -> Vec<u8> {
        let mut r = StreamRedactor::new(set(values));
        let mut out = Vec::new();
        for c in chunks {
            out.extend(r.push(c));
        }
        out.extend(r.finish_confirmed());
        out
    }

    /// A secret split across two chunks is replaced, as in one chunk.
    #[test]
    fn a_secret_split_across_chunks_is_redacted() {
        assert_eq!(
            run(&[b"token-value"], &[b"tok", b"en-value\n"]),
            b"***\n".to_vec()
        );
        let src = b"a token-value b";
        let chunks: Vec<&[u8]> = src.chunks(1).collect();
        assert_eq!(run(&[b"token-value"], &chunks), b"a *** b".to_vec());
    }

    /// Only a suffix that could start a secret is held; a line ending in
    /// anything else is emitted at once, the first one included. Kills: a
    /// suffix rule that holds the start of every part (a quiet step's first
    /// line would show only when the step ends).
    #[test]
    fn only_the_start_of_a_secret_is_held() {
        let mut r = StreamRedactor::new(set(&[b"token-value"]));
        assert_eq!(r.push(b"line one\n"), b"line one\n".to_vec());
        assert_eq!(r.push(b"abc tok"), b"abc ".to_vec());
        assert_eq!(r.push(b"io\n"), b"tokio\n".to_vec());
        assert_eq!(r.finish_confirmed(), Vec::<u8>::new());
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

    /// I1 prefix rule (772c34f): at a confirmed end, a held proper prefix
    /// of a value is `***`, never its bytes. Kills: no prefix rule (the old
    /// `finish` emitted the held bytes).
    #[test]
    fn a_confirmed_end_inside_a_value_prefix_stores_stars() {
        assert_eq!(
            run(&[b"token-value"], &[b"ends in token-va"]),
            b"ends in ***".to_vec()
        );
    }

    /// I1 suffix rule (R-1): a part that starts inside a value (a rotated
    /// log file, before the first read or after a `source_restarted` row)
    /// stores the value's tail of at least 8 bytes as `***`. Kills: no
    /// suffix rule at the start of a part.
    #[test]
    fn a_first_part_starting_inside_a_value_stores_stars() {
        let value: &[u8] = b"ghs_0123456789abcdef";
        // The part starts with the last 13 bytes of the value, split.
        assert_eq!(
            run(&[value], &[b"456", b"789abcdef rest\n"]),
            b"*** rest\n".to_vec()
        );
    }

    /// R-1: the suffix rule starts at 8 bytes. Kills: both off-by-one
    /// mutants of `MIN_SUFFIX`.
    #[test]
    fn a_start_that_ends_a_value_is_starred_from_8_bytes() {
        let value: &[u8] = b"ghs_0123456789abcdef";
        assert_eq!(
            run(&[value], &[b"9abcdef rest\n"]),
            b"9abcdef rest\n".to_vec(),
            "7 bytes of a value's end are stored"
        );
        assert_eq!(
            run(&[value], &[b"89abcdef rest\n"]),
            b"*** rest\n".to_vec(),
            "8 bytes of a value's end are `***`"
        );
    }

    /// The stated cost of the suffix rule on every part: output that
    /// begins with 8 or more bytes that end a value shows `***` there,
    /// although the step printed them itself. Pinned so the rule is not
    /// narrowed into storing a value's tail.
    #[test]
    fn output_that_begins_with_the_end_of_a_value_is_starred() {
        assert_eq!(
            run(
                &[b"deploy-to-production"],
                &[b"production deploy started\n"]
            ),
            b"*** deploy started\n".to_vec()
        );
    }

    /// 2599a7b: V1's 8-byte tail is V2's head. A part that starts with V2
    /// stores `***` however the source is cut, as in one chunk; before
    /// the fix the cut after 8 bytes stored `***MNOPQRST`, V2's last 8
    /// bytes. A part that starts with V1's tail and no longer value still
    /// gets `***` once the next bytes show V2 does not match.
    #[test]
    fn a_part_starting_with_a_value_whose_head_ends_another_stores_stars() {
        let values: &[&[u8]] = &[b"abcdefghijkl", b"efghijklMNOPQRST"];
        let src: &[u8] = b"efghijklMNOPQRST";
        assert_eq!(run(values, &[b"efghijkl", b"MNOPQRST"]), b"***".to_vec());
        for cut in 1..src.len() {
            let (a, b) = src.split_at(cut);
            assert_eq!(run(values, &[a, b]), b"***".to_vec(), "cut at {cut}");
        }
        let bytes: Vec<&[u8]> = src.chunks(1).collect();
        assert_eq!(run(values, &bytes), b"***".to_vec(), "one byte per chunk");
        assert_eq!(
            run(values, &[b"efghijkl", b" rest\n"]),
            b"*** rest\n".to_vec()
        );
    }

    /// A start held because a longer value could still match is decided
    /// at a confirmed end with the bytes held: V1's tail is `***`.
    #[test]
    fn a_held_start_at_a_confirmed_end_is_starred() {
        let mut r = StreamRedactor::new(set(&[b"abcdefghijkl", b"efghijklMNOPQRST"]));
        assert_eq!(
            r.push(b"efghijkl"),
            Vec::<u8>::new(),
            "held: V2 could match"
        );
        assert_eq!(r.finish_confirmed(), b"***".to_vec());
    }

    // -- properties of the redactor over chunkings --------------------------
    //
    // Value bytes come from `VALUE_BYTES` and filler from `FILLER`, which
    // share no byte and hold no `*`. Values are built to overlap: V1 is a
    // base, V2 starts with an 8-byte or longer tail of V1 (the 2599a7b
    // shape), and optionally V3 is a tail of V1, V4 extends V2, and one
    // free value. Three letters make further overlaps common.

    const VALUE_BYTES: &[u8] = b"abc";
    const FILLER: &[u8] = b"x \n";

    fn bytes_of(
        alphabet: &'static [u8],
        len: std::ops::RangeInclusive<usize>,
    ) -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(proptest::sample::select(alphabet), len)
    }

    fn overlapping_values() -> impl Strategy<Value = Vec<Vec<u8>>> {
        (
            bytes_of(VALUE_BYTES, 9..=16),
            any::<Index>(),
            bytes_of(VALUE_BYTES, 1..=8),
            proptest::option::of(any::<Index>()),
            proptest::option::of(bytes_of(VALUE_BYTES, 1..=4)),
            proptest::option::of(bytes_of(VALUE_BYTES, 2..=12)),
        )
            .prop_map(|(v1, k, ext, v3, v4, free)| {
                let len = v1.len();
                // A tail of V1 of 8..len bytes.
                let tail = |i: Index| v1[len - (8 + i.index(len - 8))..].to_vec();
                let v2 = [tail(k), ext].concat();
                let mut values = vec![v1.clone(), v2.clone()];
                values.extend(v3.map(tail));
                values.extend(v4.map(|t| [v2.clone(), t].concat()));
                values.extend(free);
                values
            })
    }

    /// Sorted cut points of `len` bytes, repeats allowed (empty chunks).
    fn cuts(len: usize) -> impl Strategy<Value = Vec<usize>> {
        proptest::collection::vec(0..=len, 0..=8).prop_map(|mut c| {
            c.sort_unstable();
            c
        })
    }

    fn chunked<'a>(src: &'a [u8], cuts: &[usize]) -> Vec<&'a [u8]> {
        let mut out = Vec::new();
        let mut from = 0;
        for &c in cuts {
            out.push(&src[from..c]);
            from = c;
        }
        out.push(&src[from..]);
        out
    }

    fn redacted(values: &[Vec<u8>], chunks: &[&[u8]]) -> Vec<u8> {
        let values: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
        run(&values, chunks)
    }

    /// A source of whole values between filler, which may start with a
    /// value's tail of 8 or more bytes, cut into chunks.
    fn separated_source() -> impl Strategy<Value = (Vec<Vec<u8>>, Vec<u8>, Vec<usize>)> {
        overlapping_values()
            .prop_flat_map(|values| {
                let n = values.len();
                let first = prop_oneof![
                    Just(None),
                    (0..n).prop_map(|i| Some((i, None))),
                    (any::<Index>(), any::<Index>())
                        .prop_map(move |(i, t)| Some((i.index(n), Some(t)))),
                ];
                let groups = proptest::collection::vec((bytes_of(FILLER, 1..=4), 0..n), 0..=4);
                (Just(values), first, groups, bytes_of(FILLER, 0..=4))
            })
            .prop_map(|(values, first, groups, trail)| {
                let mut src = Vec::new();
                match first {
                    None => {}
                    Some((i, None)) => src.extend_from_slice(&values[i]),
                    Some((i, Some(t))) => {
                        // A tail of 8 or more bytes of a value longer than 8.
                        let long: Vec<&Vec<u8>> = values.iter().filter(|v| v.len() > 8).collect();
                        let v = long[i % long.len()];
                        let n = 8 + t.index(v.len() - 8);
                        src.extend_from_slice(&v[v.len() - n..]);
                    }
                }
                for (filler, i) in groups {
                    src.extend(filler);
                    src.extend_from_slice(&values[i]);
                }
                src.extend(trail);
                (values, src)
            })
            .prop_flat_map(|(values, src)| {
                let len = src.len();
                (Just(values), Just(src), cuts(len))
            })
    }

    /// What the redactor must store for a [`separated_source`]: every run
    /// of value bytes is `***`, filler is copied.
    fn reference(src: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut in_value = false;
        for &b in src {
            if VALUE_BYTES.contains(&b) {
                if !in_value {
                    out.extend_from_slice(REDACTED);
                }
                in_value = true;
            } else {
                out.push(b);
                in_value = false;
            }
        }
        out
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            max_shrink_iters: 1024,
            ..ProptestConfig::default()
        })]

        /// No value byte is stored, however the source is cut: each whole
        /// value, and a part's leading value tail of 8 or more bytes, is
        /// `***`. Kills 2599a7b (V2's end stored after V1's tail) and any
        /// redactor that stores a value consistently in clear.
        #[test]
        fn any_chunking_stores_the_reference_output((values, src, cuts) in separated_source()) {
            prop_assert_eq!(redacted(&values, &chunked(&src, &cuts)), reference(&src));
        }

        /// Any cut of any source of value and filler bytes (short tails,
        /// heads, values run together) stores what the source in one chunk
        /// stores, so a re-read cut differently matches the stored digests.
        #[test]
        fn any_chunking_stores_what_one_chunk_stores(
            (values, src, cuts) in overlapping_values().prop_flat_map(|values| {
                (Just(values), bytes_of(b"abcx \n", 0..=48))
            }).prop_flat_map(|(values, src)| {
                let len = src.len();
                (Just(values), Just(src), cuts(len))
            })
        ) {
            prop_assert_eq!(
                redacted(&values, &chunked(&src, &cuts)),
                redacted(&values, &[&src])
            );
        }
    }

    /// A value starting with `***` is redacted like any other value.
    #[test]
    fn a_value_starting_with_stars_is_redacted() {
        assert_eq!(
            run(&[b"***secret"], &[b"a ***secret b\n"]),
            b"a *** b\n".to_vec()
        );
    }

    #[test]
    fn sidecar_stream_key_namespaces_by_step_and_index() {
        assert_eq!(
            sidecar_stream_key(&StepId("test".into()), 0).0,
            "test::service-0"
        );
        assert_ne!(
            sidecar_stream_key(&StepId("build".into()), 0).0,
            sidecar_stream_key(&StepId("build".into()), 1).0,
        );
    }

    /// The minted forge tokens of a ledger, once each.
    #[test]
    fn minted_tokens_are_listed_once() {
        let token = |v: &str, minted| MountedValue {
            kind: MountedValueKind::ForgeToken,
            value: v.as_bytes().to_vec(),
            repo: None,
            minted: Some(minted),
        };
        let s = ValueSet::new(
            2,
            vec![
                token("t1", true),
                token("t2", true),
                token("t1", true),
                token("ll", false),
            ],
        );
        let minted: Vec<_> = s
            .minted_forge_tokens()
            .iter()
            .map(|m| m.value.clone())
            .collect();
        assert_eq!(minted, vec![b"t1".to_vec(), b"t2".to_vec()]);
    }
}
