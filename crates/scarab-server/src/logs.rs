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
use std::sync::{Arc, Mutex, Weak};
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
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::redact::{Core, Sam, Trie};

/// Errors from the log pipeline's reads.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("db error: {0}")]
    Db(#[from] scarab_engine::DbError),
    #[error("object store error: {0}")]
    Store(#[from] scarab_storage::StorageError),
    #[error("gzip error: {0}")]
    Gzip(String),
    #[error(transparent)]
    Redactor(#[from] PrepareError),
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
pub struct ValueSet {
    gen: i64,
    entries: Vec<MountedValue>,
    values: Vec<Vec<u8>>,
    generations: Vec<LedgerGeneration>,
    /// The bytes of `values`, added up.
    value_bytes: usize,
    /// The redaction automata while a reader holds them, shared with a
    /// later load of the same generation ([`ValueSet::keep_automata_of`]).
    cells: Arc<Cells>,
    /// The process's limits on building and keeping automata.
    limits: Arc<AutomataLimits>,
}

impl std::fmt::Debug for ValueSet {
    // No value bytes: a value set is printed in traces.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValueSet")
            .field("gen", &self.gen)
            .field("values", &self.values.len())
            .field("value_bytes", &self.value_bytes)
            .finish()
    }
}

/// The automata of a value set, held weakly: they live while a reader (a
/// [`StreamRedactor`], a failure cause being redacted) holds them, and are
/// freed with their share of the budget when the last one ends. The
/// value cache keeps the values, not the automata.
#[derive(Default)]
struct Cells {
    trie: tokio::sync::Mutex<Weak<Built<Trie>>>,
    sam: tokio::sync::Mutex<Weak<Built<Sam>>>,
}

/// A built automaton and its share of the process budget, freed together.
pub(crate) struct Built<T> {
    value: Arc<T>,
    _budget: OwnedSemaphorePermit,
}

/// The default process budget for built automata: 512 MiB.
pub const AUTOMATA_BUDGET_BYTES: usize = 512 * 1024 * 1024;

/// How many value sets the process builds automata for at once.
pub const CONCURRENT_BUILDS: usize = 2;

/// The process's limits on the redaction automata. A build first reserves
/// the most its automata can take (52 bytes per value byte) from the
/// budget, waiting while the budget is full, then takes one of
/// [`CONCURRENT_BUILDS`] build slots, which bounds the memory a build uses
/// only while it runs. Nothing is ever redacted without its automata.
pub(crate) struct AutomataLimits {
    /// KiB of built automata.
    budget: Arc<Semaphore>,
    budget_kib: usize,
    builds: Semaphore,
}

impl AutomataLimits {
    fn new(budget_bytes: usize) -> Self {
        let budget_kib = budget_bytes.div_ceil(1024).max(1);
        Self {
            budget: Arc::new(Semaphore::new(budget_kib)),
            budget_kib,
            builds: Semaphore::new(CONCURRENT_BUILDS),
        }
    }

    /// Reserve `bytes` of the budget, waiting for room. A reservation over
    /// the whole budget takes the whole budget.
    async fn reserve(&self, bytes: usize) -> Result<OwnedSemaphorePermit, PrepareError> {
        let kib = bytes.div_ceil(1024).clamp(1, self.budget_kib);
        self.budget
            .clone()
            .acquire_many_owned(kib as u32)
            .await
            .map_err(|e| PrepareError::Build(e.to_string()))
    }
}

/// The most bytes a built trie of `n` value bytes keeps: at most `n + 1`
/// nodes of 21 bytes (an offset, an edge byte and target, the failure
/// link, `best` and `pend`).
fn trie_bound(n: usize) -> usize {
    21 * n + 64
}

/// The most bytes a built suffix automaton of `n` value bytes keeps: at
/// most `2n` states of 8 bytes and `3n` transitions of 5.
fn sam_bound(n: usize) -> usize {
    31 * n + 64
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
            Arc::new(AutomataLimits::new(AUTOMATA_BUDGET_BYTES)),
        )
    }

    fn from_generations(
        gen: i64,
        generations: Vec<LedgerGeneration>,
        limits: Arc<AutomataLimits>,
    ) -> Self {
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
        let value_bytes = values.iter().map(Vec::len).sum();
        Self {
            gen,
            entries,
            values,
            generations,
            value_bytes,
            cells: Arc::default(),
            limits,
        }
    }

    /// Share `other`'s automata (while a reader holds them) when it is the
    /// same generation with the same values.
    fn keep_automata_of(&mut self, other: &ValueSet) {
        if other.gen == self.gen && other.values == self.values {
            self.cells = other.cells.clone();
        }
    }

    /// The bytes of every value, added up.
    pub fn value_bytes(&self) -> usize {
        self.value_bytes
    }

    fn check_size(&self) -> Result<(), ValuesTooLarge> {
        if self.value_bytes > MAX_VALUE_BYTES {
            return Err(ValuesTooLarge {
                bytes: self.value_bytes,
            });
        }
        Ok(())
    }

    /// Build the automata `want` asks for (trie, suffix automaton) in one
    /// of the process's build slots: on a blocking thread when the values
    /// are over 16 KiB, else here. A build that did not finish (a panic,
    /// the runtime shutting down) is an error: nothing is built inline on
    /// an async worker in its place.
    async fn build(
        self: &Arc<Self>,
        want: (bool, bool),
    ) -> Result<(Option<Trie>, Option<Sam>), PrepareError> {
        let _slot = self
            .limits
            .builds
            .acquire()
            .await
            .map_err(|e| PrepareError::Build(e.to_string()))?;
        let build = move |values: &[Vec<u8>]| {
            (
                want.0.then(|| Trie::build(values)),
                want.1.then(|| Sam::build(values)),
            )
        };
        if self.value_bytes <= BUILD_INLINE_BYTES {
            return Ok(build(&self.values));
        }
        let set = self.clone();
        tokio::task::spawn_blocking(move || build(&set.values))
            .await
            .map_err(|e| PrepareError::Build(format!("the automata build did not finish: {e}")))
    }

    /// The trie, built (within the budget) if no reader holds it. What
    /// [`redact_text_with`] needs.
    pub(crate) async fn text_automaton(self: &Arc<Self>) -> Result<Arc<Built<Trie>>, PrepareError> {
        self.check_size()?;
        let mut cell = self.cells.trie.lock().await;
        if let Some(t) = cell.upgrade() {
            return Ok(t);
        }
        let budget = self.limits.reserve(trie_bound(self.value_bytes)).await?;
        let (trie, _) = self.build((true, false)).await?;
        let built = Arc::new(Built {
            value: Arc::new(trie.expect("asked for")),
            _budget: budget,
        });
        *cell = Arc::downgrade(&built);
        Ok(built)
    }

    /// The trie and the suffix automaton, built (within the budget) if no
    /// reader holds them. Both are reserved in one wait, so two readers
    /// never each hold half of what they need while the budget is full.
    async fn stream_automata(
        self: &Arc<Self>,
    ) -> Result<(Arc<Built<Trie>>, Arc<Built<Sam>>), PrepareError> {
        self.check_size()?;
        // Always the trie's lock first, as in `text_automaton`.
        let mut tcell = self.cells.trie.lock().await;
        let mut scell = self.cells.sam.lock().await;
        let (t, s) = (tcell.upgrade(), scell.upgrade());
        if let (Some(t), Some(s)) = (&t, &s) {
            return Ok((t.clone(), s.clone()));
        }
        let n = self.value_bytes;
        let need_t = if t.is_none() { trie_bound(n) } else { 0 };
        let need_s = if s.is_none() { sam_bound(n) } else { 0 };
        let budget = self.limits.reserve(need_t + need_s).await?;
        let (trie, sam) = self.build((t.is_none(), s.is_none())).await?;
        // Each built automaton keeps its own share of the reservation, so
        // either can be freed first.
        let mut budget = Some(budget);
        let t = match t {
            Some(t) => t,
            None => {
                let own = if s.is_none() {
                    let b = budget.as_mut().expect("not taken yet");
                    let share = need_t.div_ceil(1024).min(b.num_permits());
                    b.split(share).expect("the share is within the reservation")
                } else {
                    budget.take().expect("not taken yet")
                };
                let built = Arc::new(Built {
                    value: Arc::new(trie.expect("asked for")),
                    _budget: own,
                });
                *tcell = Arc::downgrade(&built);
                built
            }
        };
        let s = match s {
            Some(s) => s,
            None => {
                let built = Arc::new(Built {
                    value: Arc::new(sam.expect("asked for")),
                    _budget: budget.take().expect("the rest of the reservation"),
                });
                *scell = Arc::downgrade(&built);
                built
            }
        };
        Ok((t, s))
    }

    /// Which automata a reader holds now: (trie, suffix automaton).
    #[cfg(test)]
    fn built(&self) -> (bool, bool) {
        fn alive<T>(c: &tokio::sync::Mutex<Weak<T>>) -> bool {
            c.try_lock().map(|w| w.strong_count() > 0).unwrap_or(true)
        }
        (alive(&self.cells.trie), alive(&self.cells.sam))
    }

    /// Whether `other` shares this set's automata.
    #[cfg(test)]
    fn shares_automata_with(&self, other: &ValueSet) -> bool {
        Arc::ptr_eq(&self.cells, &other.cells)
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
    /// The process's budget and build slots for redaction automata: one
    /// `LogService` per server process.
    limits: Arc<AutomataLimits>,
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
            limits: Arc::new(AutomataLimits::new(AUTOMATA_BUDGET_BYTES)),
        }
    }

    /// Keep at most `bytes` of built redaction automata in this process
    /// (default [`AUTOMATA_BUDGET_BYTES`]). A read whose automata do not
    /// fit waits until a reader ends; it is never read without them.
    pub fn with_automata_budget(mut self, bytes: usize) -> Self {
        self.limits = Arc::new(AutomataLimits::new(bytes));
        self
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
            let key = unit_key(unit);
            let mut cache = self.cache.lock().unwrap();
            // An expired entry of this unit stays until the reload, which
            // keeps its automata when the generation has not moved.
            cache.retain(|k, c| *k == key || c.at.elapsed() < VALUES_TTL);
            if let Some(c) = cache.get(&key) {
                if c.at.elapsed() < VALUES_TTL {
                    return Ok(c.set.clone());
                }
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
        let mut set = ValueSet::from_generations(gen, generations, self.limits.clone());
        let mut cache = self.cache.lock().unwrap();
        if let Some(c) = cache.get(&unit_key(unit)) {
            set.keep_automata_of(&c.set);
        }
        let set = Arc::new(set);
        cache.insert(
            unit_key(unit),
            Cached {
                set: set.clone(),
                at: Instant::now(),
            },
        );
        drop(cache);
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
            Ok(set) => match set.text_automaton().await {
                Ok(trie) => redact_text_with(&trie, text),
                Err(_) => CAUSE_NOT_STORED.to_string(),
            },
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

    /// A redactor for one read of a part of `values`' unit. Its automata
    /// are built within the process budget, waiting for room, off the
    /// async workers when large. A set over [`MAX_VALUE_BYTES`] gets none
    /// (its log is not stored); a build that fails is an error (the read
    /// fails and is retried).
    pub async fn stream_redactor(
        &self,
        values: Arc<ValueSet>,
    ) -> Result<StreamRedactor, PrepareError> {
        StreamRedactor::prepared(values, MIN_SUFFIX).await
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
        let mut redactor = self.stream_redactor(values.clone()).await?;
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

/// The shortest start of a part that is redacted because it ends a value,
/// and the shortest part redacted because it lies inside one (residual
/// R-1: a shorter one is stored).
pub const MIN_SUFFIX: usize = 8;

/// The most value bytes (every value of a unit, added up) a log is
/// redacted with. A unit over it has its log not stored
/// ([`NotStoredReason::ValuesTooLarge`]) and its failure causes replaced
/// with [`CAUSE_NOT_STORED`]: a value is never redacted in part.
///
/// Why 256 KiB: the automata keep at most 52 bytes per value byte (21 for
/// the trie, at most 31 for the suffix automaton at 2 states and 3
/// transitions per byte; 44 measured on random bytes), so at most 13 MiB
/// per unit being read, and about 36 MiB while the suffix automaton is
/// built (its lookup map). A PEM key is about 3 KiB, a kubeconfig under
/// 10 KiB, the Mozilla CA bundle about 220 KiB.
pub const MAX_VALUE_BYTES: usize = 256 * 1024;

/// A value set over this many bytes is built on a blocking thread, not on
/// an async worker.
const BUILD_INLINE_BYTES: usize = 16 * 1024;

/// A unit's values are over [`MAX_VALUE_BYTES`]: nothing is redacted with
/// them and nothing they cover is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the unit's secret values are {bytes} bytes in all, over the {MAX_VALUE_BYTES} bytes a log is redacted with")]
pub struct ValuesTooLarge {
    pub bytes: usize,
}

/// Why a value set has no automata for a reader.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrepareError {
    /// Over [`MAX_VALUE_BYTES`], for every replica: the log is not stored.
    #[error(transparent)]
    TooLarge(#[from] ValuesTooLarge),
    /// The build did not finish here (a panic, the runtime shutting down):
    /// local to this replica, the read is retried.
    #[error("{0}")]
    Build(String),
}

/// Redact a whole `text` with the trie of a value set
/// ([`ValueSet::text_automaton`]): every occurrence of a value, with
/// overlapping ones as one `***`. The text is known whole, so its start and
/// end are not cuts.
pub(crate) fn redact_text_with(trie: &Built<Trie>, text: &str) -> String {
    let mut core = Core::text(trie.value.clone());
    let mut out = core.push(text.as_bytes());
    out.extend(core.finish(false));
    String::from_utf8_lossy(&out).into_owned()
}

/// [`redact_text_with`], building the trie on this thread; a set over
/// [`MAX_VALUE_BYTES`] gives [`CAUSE_NOT_STORED`]. Small value sets only.
#[cfg(test)]
fn redact_text(values: &Arc<ValueSet>, text: &str) -> String {
    match futures::executor::block_on(values.text_automaton()) {
        Ok(trie) => redact_text_with(&trie, text),
        Err(_) => CAUSE_NOT_STORED.to_string(),
    }
}

/// Redacts one read of a part of a log source as a stream of chunks, with
/// the values of one ledger generation (a new generation needs a new
/// redactor). ADR-0082 §3: it stores the union of every match.
///
/// What it stores for the bytes `S` it reads: every occurrence of every
/// value; the longest start of `S` of at least [`MIN_SUFFIX`] bytes that
/// ends a value (a read can start inside a value when the kubelet rotated
/// the log file); at a confirmed end, all of `S` when it is
/// [`MIN_SUFFIX`] bytes or longer and lies inside a value with more of the
/// value after it, and the shortest end of `S` that begins a value. Marked
/// ranges that share a byte are one `***`; ranges that only touch are two.
///
/// [`push`](Self::push) emits what no later byte can change, so what it
/// has emitted is a prefix of the output of every continuation of the
/// source: how the source is cut changes when bytes are emitted, never
/// which. After each push it holds at most the longest value less one
/// byte. A read that fails drops the redactor and its held bytes: they may
/// be part of a secret, and the next read sees them again.
pub struct StreamRedactor {
    values: Arc<ValueSet>,
    core: Core,
    /// Holds the automata (and their share of the budget) while the read
    /// lasts; the last redactor of a set to end frees them.
    _automata: (Arc<Built<Trie>>, Arc<Built<Sam>>),
}

impl StreamRedactor {
    /// A redactor whose automata are built within the process budget
    /// ([`LogService::stream_redactor`]).
    async fn prepared(values: Arc<ValueSet>, min_suffix: usize) -> Result<Self, PrepareError> {
        let (trie, sam) = values.stream_automata().await?;
        let core = Core::stream(trie.value.clone(), sam.value.clone(), min_suffix);
        Ok(Self {
            values,
            core,
            _automata: (trie, sam),
        })
    }

    /// A redactor for one read, its automata built on this thread. Small
    /// value sets only.
    #[cfg(test)]
    fn new(values: Arc<ValueSet>) -> Result<Self, PrepareError> {
        Self::with_min_suffix(values, MIN_SUFFIX)
    }

    /// [`new`](Self::new) with another minimum than [`MIN_SUFFIX`], for
    /// tests that search small inputs.
    #[cfg(test)]
    fn with_min_suffix(values: Arc<ValueSet>, min_suffix: usize) -> Result<Self, PrepareError> {
        futures::executor::block_on(Self::prepared(values, min_suffix))
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
        self.core.push(src)
    }

    /// The source's end is confirmed: emit the held bytes, with the
    /// whole-part and trailing ranges as `***`.
    pub fn finish_confirmed(self) -> Vec<u8> {
        self.core.finish(true)
    }

    /// The bytes held now.
    #[cfg(test)]
    fn held_len(&self) -> usize {
        self.core.held_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::sample::Index;
    use scarab_engine::{MountedValueKind, StepId};
    use std::collections::HashMap;

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
        let mut r = StreamRedactor::new(set(values)).unwrap();
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
        let mut r = StreamRedactor::new(set(&[b"token-value"])).unwrap();
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
        let mut r = StreamRedactor::new(set(&[b"abcdefghijkl", b"efghijklMNOPQRST"])).unwrap();
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
    fn separated_reference(src: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut in_value = false;
        for &b in src {
            if VALUE_BYTES.contains(&b) {
                if !in_value {
                    out.extend_from_slice(b"***");
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
            prop_assert_eq!(redacted(&values, &chunked(&src, &cuts)), separated_reference(&src));
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

    // -- ADR-0082 §3: the union of matches ----------------------------------
    //
    // `reference` is a brute force of §3's definition: no automaton. Every
    // window against every value, every leading length, every whole-part
    // and trailing start; intervals that share a byte merge, touching ones
    // do not.

    fn set_of(values: &[Vec<u8>]) -> Arc<ValueSet> {
        let v: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
        set(&v)
    }

    /// The marked intervals of `src`. `lead`: a stream (the leading and,
    /// at a confirmed end, the whole-part interval); `confirmed`: a
    /// confirmed end (the trailing interval).
    fn intervals(
        values: &[Vec<u8>],
        src: &[u8],
        lead: bool,
        confirmed: bool,
        min: usize,
    ) -> Vec<(usize, usize)> {
        let vals: Vec<&[u8]> = values
            .iter()
            .map(Vec::as_slice)
            .filter(|v| !v.is_empty())
            .collect();
        let mut iv = Vec::new();
        for v in &vals {
            for i in 0..(src.len() + 1).saturating_sub(v.len()) {
                if &src[i..i + v.len()] == *v {
                    iv.push((i, i + v.len()));
                }
            }
        }
        if lead {
            let n = vals
                .iter()
                .flat_map(|v| {
                    (min..=v.len().min(src.len())).filter(move |&m| src[..m] == v[v.len() - m..])
                })
                .max();
            if let Some(n) = n {
                iv.push((0, n));
            }
            let inside = vals.iter().any(|v| {
                v.len() > src.len() && (0..v.len() - src.len()).any(|i| &v[i..i + src.len()] == src)
            });
            if confirmed && src.len() >= min && inside {
                iv.push((0, src.len()));
            }
        }
        if confirmed {
            let p = (0..src.len()).find(|&p| {
                vals.iter()
                    .any(|v| v.len() > src.len() - p && v.starts_with(&src[p..]))
            });
            if let Some(p) = p {
                iv.push((p, src.len()));
            }
        }
        iv
    }

    fn render(src: &[u8], mut iv: Vec<(usize, usize)>) -> Vec<u8> {
        iv.sort_unstable();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (s, e) in iv {
            match spans.last_mut() {
                Some(last) if s < last.1 => last.1 = last.1.max(e),
                _ => spans.push((s, e)),
            }
        }
        let mut out = Vec::new();
        let mut i = 0;
        for (s, e) in spans {
            out.extend_from_slice(&src[i..s]);
            out.extend_from_slice(b"***");
            i = e;
        }
        out.extend_from_slice(&src[i..]);
        out
    }

    /// What a stream redactor with minimum `min` stores for `src` up to a
    /// confirmed end.
    fn reference(values: &[Vec<u8>], src: &[u8], min: usize) -> Vec<u8> {
        render(src, intervals(values, src, true, true, min))
    }

    /// What one read records: the output and, after each push, the bytes
    /// read, everything emitted so far and the bytes held.
    struct Recorded {
        out: Vec<u8>,
        pushes: Vec<(usize, Vec<u8>, usize)>,
    }

    fn record(values: &Arc<ValueSet>, chunks: &[&[u8]], min: usize) -> Recorded {
        let mut r = StreamRedactor::with_min_suffix(values.clone(), min).unwrap();
        let mut out = Vec::new();
        let mut read = 0;
        let mut pushes = Vec::new();
        for c in chunks {
            out.extend(r.push(c));
            read += c.len();
            pushes.push((read, out.clone(), r.held_len()));
        }
        out.extend(r.finish_confirmed());
        Recorded { out, pushes }
    }

    /// Every check of one source against the reference: the output of one
    /// byte per push and of `chunks` equals it; after every push of
    /// either, what was emitted is a prefix of the reference of the whole
    /// source and of the bytes read so far with a confirmed end, it is
    /// what one byte per push had emitted after the same bytes, and at
    /// most the longest value less one byte is held.
    fn check_source(
        values: &[Vec<u8>],
        set: &Arc<ValueSet>,
        src: &[u8],
        chunks: &[&[u8]],
        min: usize,
    ) -> Result<(), String> {
        let want = reference(values, src, min);
        let longest = values.iter().map(Vec::len).max().unwrap_or(0);
        let one: Vec<&[u8]> = src.chunks(1).collect();
        // Holds the automata, so each run reuses them (they are freed with
        // their last reader).
        let _keep = StreamRedactor::with_min_suffix(set.clone(), min).unwrap();
        let base = record(set, &one, min);
        let by_read: HashMap<usize, Vec<u8>> = base
            .pushes
            .iter()
            .map(|(k, e, _)| (*k, e.clone()))
            .collect();
        for rec in [&base, &record(set, chunks, min)] {
            if rec.out != want {
                return Err(format!(
                    "stored {:?}, reference {:?}",
                    String::from_utf8_lossy(&rec.out),
                    String::from_utf8_lossy(&want)
                ));
            }
            for (k, emitted, held) in &rec.pushes {
                if !want.starts_with(emitted)
                    || !reference(values, &src[..*k], min).starts_with(emitted)
                {
                    return Err(format!("after {k} bytes emitted {emitted:?}"));
                }
                if *k > 0 && by_read.get(k).is_some_and(|b| b != emitted) {
                    return Err(format!(
                        "after {k} bytes the emitted bytes depend on the cut"
                    ));
                }
                if *held > longest.saturating_sub(1) {
                    return Err(format!("{held} bytes held after {k}"));
                }
            }
        }
        Ok(())
    }

    /// A small xorshift generator: the property tests draw a seed and
    /// build value sets and sources of chosen shapes from it.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
        fn range(&mut self, lo: usize, hi: usize) -> usize {
            lo + self.below(hi - lo + 1)
        }
        fn word(&mut self, alphabet: &[u8], lo: usize, hi: usize) -> Vec<u8> {
            let n = self.range(lo, hi);
            (0..n)
                .map(|_| alphabet[self.below(alphabet.len())])
                .collect()
        }
    }

    /// `a` then `b`, sharing the longest end of `a` that begins `b`.
    fn glue(a: &[u8], b: &[u8]) -> Vec<u8> {
        let k = (0..=a.len().min(b.len()))
            .rev()
            .find(|&k| a.ends_with(&b[..k]))
            .unwrap_or(0);
        [a, &b[k..]].concat()
    }

    /// A value set of one of seven shapes (free values, a prefix chain, a
    /// suffix chain, repeats of a word, infixes of one value, lengths
    /// `min - 1`, `min`, `min + 1` and 1, rotations) over a one- to
    /// three-letter alphabet, some with `*` or `\n`.
    fn gen_values(rng: &mut Rng, min: usize) -> (Vec<Vec<u8>>, &'static [u8]) {
        const ALPHABETS: [&[u8]; 6] = [b"a", b"ab", b"a*", b"a\n", b"abc", b"ab*\n"];
        let a = ALPHABETS[rng.below(ALPHABETS.len())];
        let hi = if min < 8 { 6 } else { 20 };
        let values = match rng.below(7) {
            0 => (0..rng.range(1, 4)).map(|_| rng.word(a, 1, hi)).collect(),
            1 => {
                let v = rng.word(a, min + 1, hi + 4);
                (0..rng.range(1, 4))
                    .map(|_| v[..rng.range(1, v.len())].to_vec())
                    .collect()
            }
            2 => {
                let v = rng.word(a, min + 1, hi + 4);
                (0..rng.range(1, 4))
                    .map(|_| v[rng.below(v.len())..].to_vec())
                    .collect()
            }
            3 => {
                let u = rng.word(a, 1, 3);
                vec![
                    u.repeat(rng.range(1, 8)),
                    u.repeat(rng.range(1, 8)),
                    rng.word(a, 1, hi),
                ]
            }
            4 => {
                let v = rng.word(a, min + 2, hi + 6);
                let mut out = vec![v.clone()];
                for _ in 0..rng.range(1, 3) {
                    let i = rng.below(v.len());
                    let j = rng.range(i + 1, v.len());
                    out.push(v[i..j].to_vec());
                }
                out
            }
            5 => vec![
                rng.word(a, min, min),
                rng.word(
                    a,
                    min.saturating_sub(1).max(1),
                    min.saturating_sub(1).max(1),
                ),
                rng.word(a, min + 1, min + 1),
                rng.word(a, 1, 1),
            ],
            _ => {
                let v = rng.word(a, min + 1, hi);
                let j = rng.range(1, v.len() - 1);
                let rot = [&v[j..], &v[..j]].concat();
                let tail = [&v[j..], &rng.word(a, 1, hi)[..]].concat();
                vec![v, rot, tail]
            }
        };
        (values, a)
    }

    /// A source of random bytes, or values glued with their overlap
    /// shared, that may start inside a value (a tail or an infix) and end
    /// inside one, with filler `x` between.
    fn gen_source(rng: &mut Rng, values: &[Vec<u8>], alphabet: &[u8]) -> Vec<u8> {
        if rng.below(4) == 0 {
            let mut a = alphabet.to_vec();
            a.push(b'x');
            return rng.word(&a, 0, 40);
        }
        let mut src = Vec::new();
        if rng.below(10) < 6 {
            let v = &values[rng.below(values.len())];
            let i = rng.below(v.len());
            src = if rng.below(10) < 6 {
                v[i..].to_vec()
            } else {
                v[i..rng.range(i + 1, v.len())].to_vec()
            };
        }
        for _ in 0..rng.range(0, 4) {
            if rng.below(10) < 3 {
                src.push(b'x');
            }
            let mut v = values[rng.below(values.len())].clone();
            if rng.below(10) < 3 {
                v = v[rng.below(v.len())..].to_vec();
            }
            if rng.below(10) < 3 {
                v.truncate(rng.range(1, v.len()));
            }
            src = if rng.below(10) < 7 {
                glue(&src, &v)
            } else {
                [src, v].concat()
            };
        }
        src
    }

    /// Cuts of `src` at random points, with empty chunks.
    fn gen_chunks<'a>(rng: &mut Rng, src: &'a [u8]) -> Vec<&'a [u8]> {
        let mut cuts: Vec<usize> = (0..rng.below(11))
            .map(|_| rng.below(src.len() + 1))
            .collect();
        cuts.sort_unstable();
        chunked(src, &cuts)
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 512,
            ..ProptestConfig::default()
        })]

        /// ADR-0082 §3: for value sets built to overlap and sources glued
        /// from them, with the minimum at 8 and lowered to 1, 2 or 3, the
        /// redactor stores the brute-force reference however the source is
        /// cut; after every push its output is a prefix of the reference of
        /// every continuation, the same for every cut, and it holds at most
        /// the longest value less one byte. Kills: deciding one match and
        /// skipping past it (2599a7b, 32c3c10); a span closed while a value
        /// starting before `h` could extend it; touching spans merged; the
        /// whole-part interval missing.
        #[test]
        fn the_redactor_stores_the_union_of_matches(seed in any::<u64>(), m in 0usize..4) {
            let min = [1, 2, 3, 8][m];
            let mut rng = Rng(seed | 1);
            let (values, alphabet) = gen_values(&mut rng, min);
            let src = gen_source(&mut rng, &values, alphabet);
            let chunks = gen_chunks(&mut rng, &src);
            let set = set_of(&values);
            if let Err(e) = check_source(&values, &set, &src, &chunks, min) {
                prop_assert!(false, "values {:?} source {:?} chunks {:?}: {}", values, src, chunks, e);
            }
            // A whole text: occurrences only.
            let text = String::from_utf8(src.clone()).expect("ASCII alphabets");
            prop_assert_eq!(
                redact_text(&set, &text).into_bytes(),
                render(&src, intervals(&values, &src, false, false, min))
            );
        }
    }

    /// Every set of one or two values of 1 to 4 bytes over `ab` against
    /// every source of up to 8 bytes over `ab`, with the minimum lowered to
    /// 2: one byte per push, the whole source in one push and a cut in the
    /// middle, every check of [`check_source`]. 465 sets by 511 sources;
    /// the references are computed once per set. Kills what the property
    /// test can miss by sampling.
    #[test]
    fn every_small_value_set_and_source_stores_the_reference() {
        const MIN: usize = 2;
        let words: Vec<Vec<u8>> = (1..=4usize)
            .flat_map(|n| {
                (0..1usize << n).map(move |bits| {
                    (0..n)
                        .map(|i| {
                            if bits >> (n - 1 - i) & 1 == 1 {
                                b'b'
                            } else {
                                b'a'
                            }
                        })
                        .collect()
                })
            })
            .collect();
        let sources: Vec<Vec<u8>> = (0..=8usize)
            .flat_map(|n| {
                (0..1usize << n).map(move |bits| {
                    (0..n)
                        .map(|i| {
                            if bits >> (n - 1 - i) & 1 == 1 {
                                b'b'
                            } else {
                                b'a'
                            }
                        })
                        .collect()
                })
            })
            .collect();
        // The index of a source in `sources`: by length, then by bits.
        let index = |s: &[u8]| -> usize {
            let bits = s
                .iter()
                .fold(0usize, |acc, &b| acc << 1 | usize::from(b == b'b'));
            (1 << s.len()) - 1 + bits
        };
        let mut sets: Vec<Vec<Vec<u8>>> = words.iter().map(|w| vec![w.clone()]).collect();
        for i in 0..words.len() {
            for j in i + 1..words.len() {
                sets.push(vec![words[i].clone(), words[j].clone()]);
            }
        }
        assert_eq!(sets.len(), 465);
        let mut runs = 0usize;
        for values in &sets {
            let set = set_of(values);
            let _keep = StreamRedactor::with_min_suffix(set.clone(), MIN).unwrap();
            let longest = values.iter().map(Vec::len).max().unwrap();
            let refs: Vec<Vec<u8>> = sources.iter().map(|s| reference(values, s, MIN)).collect();
            for src in &sources {
                let want = &refs[index(src)];
                let one: Vec<&[u8]> = src.chunks(1).collect();
                let base = record(&set, &one, MIN);
                assert_eq!(&base.out, want, "{values:?} on {src:?}");
                for (k, emitted, held) in &base.pushes {
                    assert!(want.starts_with(emitted), "{values:?} on {src:?} at {k}");
                    assert!(
                        refs[index(&src[..*k])].starts_with(emitted),
                        "{values:?} on {src:?}: confirmed end at {k}"
                    );
                    assert!(*held < longest, "{values:?} on {src:?}: {held} held at {k}");
                }
                assert_eq!(
                    &record(&set, &[src], MIN).out,
                    want,
                    "{values:?} on {src:?}"
                );
                let (a, b) = src.split_at(src.len() / 2);
                let cut = record(&set, &[a, b], MIN);
                assert_eq!(&cut.out, want, "{values:?} on {a:?}|{b:?}");
                if !a.is_empty() {
                    assert_eq!(
                        cut.pushes[0].1,
                        base.pushes[a.len() - 1].1,
                        "{values:?} on {a:?}|{b:?}"
                    );
                }
                runs += 3;
            }
        }
        assert_eq!(runs, 465 * 511 * 3);
    }

    /// The breaks the 0.7.0 code and §1 stored a value's bytes for (ADR-0082
    /// §3 table, 2599a7b, 32c3c10) store one `***`, in one chunk, one byte
    /// per chunk and at every two-chunk cut.
    #[test]
    fn the_breaks_of_one_match_at_a_time_store_one_run_of_stars() {
        type Case<'a> = (&'a [&'a [u8]], &'a [u8], &'a [u8]);
        let cases: [Case; 4] = [
            (
                &[b"abcdefghijkl", b"efghijklMNOPQRST"],
                b"efghijklMNOP",
                b"***",
            ),
            (
                &[b"abcdefghijkl", b"ijklXYZW1234"],
                b"efghijklXYZW1234 rest\n",
                b"*** rest\n",
            ),
            (
                &[b"user:hunter2pw", b"hunter2pw-and-more-secret"],
                b"dsn=user:hunter2pw-and-more-secret\n",
                b"dsn=***\n",
            ),
            (
                &[b"abcdefghijkl", b"efghijklMNOPQRST"],
                b"cdefghijklMNOPQRST",
                b"***",
            ),
        ];
        for (values, src, want) in cases {
            let owned: Vec<Vec<u8>> = values.iter().map(|v| v.to_vec()).collect();
            assert_eq!(reference(&owned, src, MIN_SUFFIX), want, "reference");
            assert_eq!(run(values, &[src]), want, "{src:?}");
            let bytes: Vec<&[u8]> = src.chunks(1).collect();
            assert_eq!(run(values, &bytes), want, "{src:?} one byte per chunk");
            for cut in 1..src.len() {
                let (a, b) = src.split_at(cut);
                assert_eq!(run(values, &[a, b]), want, "{src:?} cut at {cut}");
            }
        }
        // The overlap leak in the middle of a log, before 0.7.0 too.
        assert_eq!(
            redact_text(
                &set(&[b"user:hunter2pw", b"hunter2pw-and-more-secret"]),
                "dsn=user:hunter2pw-and-more-secret"
            ),
            "dsn=***"
        );
    }

    /// The whole-part interval: a part that lies wholly inside a value (a
    /// rotation between two lines of a multi-line value, and the process
    /// ended) is `***` from 8 bytes; 7 bytes are stored (R-1). Kills: no
    /// whole-part interval; it applied below the minimum.
    #[test]
    fn a_part_wholly_inside_a_value_is_starred_from_8_bytes() {
        let value: &[u8] = b"0123456789abcdefghij";
        assert_eq!(run(&[value], &[b"456789abcdef"]), b"***");
        assert_eq!(run(&[value], &[b"4567", b"89ab", b"cdef"]), b"***");
        assert_eq!(run(&[value], &[b"3456789"]), b"3456789");
        let pem: &[u8] =
            b"-----BEGIN KEY-----\nMIIEvQIBADANBgkq\nhkiG9w0BAQEFAASC\nBKcwggSjAgEAAoIB\n-----END KEY-----\n";
        assert_eq!(
            run(&[pem], &[b"MIIEvQIBADANBgkq\n", b"hkiG9w0BAQEFAASC\n"]),
            b"***"
        );
        // Residual, as in 0.7.0: value bytes that neither end the value nor
        // end the part (more output follows them) are no interval.
        assert_eq!(
            run(&[value], &[b"456789abcdef rest\n"]),
            b"456789abcdef rest\n"
        );
    }

    /// Spans that only touch stay two `***` (0.7.0 stored them so, and
    /// merging would change the digests of ordinary logs), also when the
    /// first is open, a leading interval, or the second a trailing one.
    #[test]
    fn touching_spans_stay_separate() {
        assert_eq!(run(&[b"abc", b"def"], &[b"abcdef\n"]), b"******\n");
        assert_eq!(run(&[b"tok"], &[b"toktok\n"]), b"******\n");
        assert_eq!(run(&[b"ab", b"abX", b"cd"], &[b"ab", b"cd"]), b"******");
        assert_eq!(
            run(&[b"XXXXabcdefgh", b"tok"], &[b"abcdefghtok!"]),
            b"******!"
        );
        assert_eq!(run(&[b"tok", b"abZ"], &[b"tokab"]), b"******");
        // A leading interval that absorbs two spans and is extended by a
        // third is one run.
        assert_eq!(
            run(
                &[b"XXabcdefghij", b"bc", b"ef", b"ijKL"],
                &[b"abcdefghijKL."]
            ),
            b"***."
        );
    }

    /// Intervals that share a byte are one `***`: overlapping occurrences
    /// of one value, a chain of values, and a value that repeats a word
    /// (`abab` printed twice is one run; 0.7.0 stored `******`).
    #[test]
    fn overlapping_values_store_one_run() {
        assert_eq!(run(&[b"aa"], &[b"aaa\n"]), b"***\n");
        assert_eq!(run(&[b"abc", b"bcde", b"de"], &[b"abcde\n"]), b"***\n");
        assert_eq!(run(&[b"abab"], &[b"x abababab\n"]), b"x ***\n");
        assert_eq!(run(&[b"aaaa"], &[b"x aaaaaaaaa\n"]), b"x ***\n");
        assert_eq!(
            run(&[b"abc", b"abcdef", b"cde"], &[b"x abcde", b"y"]),
            b"x ***y"
        );
    }

    /// A set over [`MAX_VALUE_BYTES`] redacts nothing and builds nothing:
    /// no stream redactor, and a text is replaced with
    /// [`CAUSE_NOT_STORED`]. A set at the cap is allowed. Kills: a cap
    /// that redacts the first bytes of a value and stores the rest.
    #[test]
    fn a_set_over_the_cap_redacts_nothing_and_builds_nothing() {
        let big = vec![b'q'; MAX_VALUE_BYTES - 3];
        let over = set(&[&big, b"tok1"]);
        assert_eq!(over.value_bytes(), MAX_VALUE_BYTES + 1);
        assert_eq!(
            StreamRedactor::new(over.clone()).err(),
            Some(PrepareError::TooLarge(ValuesTooLarge {
                bytes: MAX_VALUE_BYTES + 1
            }))
        );
        assert_eq!(redact_text(&over, "a tok1 b"), CAUSE_NOT_STORED);
        assert_eq!(over.built(), (false, false));
        let at = set(&[&big, b"tok"]);
        assert_eq!(at.value_bytes(), MAX_VALUE_BYTES);
        assert!(at.check_size().is_ok());
    }

    /// A log service over in-memory stores and the ledgers of `units`
    /// (one value each), with an automata budget of `budget` bytes.
    async fn service_with(
        budget: usize,
        units: &[(&str, &[u8])],
    ) -> (LogService, Vec<LogStreamId>) {
        use scarab_engine::{AttemptId, RunId, RunStatus};
        let db = Arc::new(scarab_testkit::InMemoryDb::new());
        let run_id = RunId("r1".into());
        db.seed_run(&run_id, RunStatus::Running);
        let store = Arc::new(scarab_testkit::InMemoryObjectStore::default());
        let logs = LogService::new(store, db.clone() as Arc<dyn Db>).with_automata_budget(budget);
        let mut ids = Vec::new();
        for (step, value) in units {
            let unit = LogStreamId::new(&run_id, &StepId((*step).into()), &AttemptId("a1".into()));
            logs.record_launch_values(
                &unit,
                Some(&format!("pod-{step}")),
                vec![MountedValue {
                    kind: MountedValueKind::Secret,
                    value: value.to_vec(),
                    repo: None,
                    minted: None,
                }],
                Timestamp(0),
            )
            .await
            .unwrap();
            ids.push(unit);
        }
        (logs, ids)
    }

    /// The automata live only while a reader holds them: a launch builds
    /// none, a failure cause builds the trie only and frees it, a stream
    /// redactor holds both (shared with a reload of the same generation)
    /// and the last one to end frees them and their budget, while the
    /// cache keeps the values. Kills: building in `from_generations`; the
    /// suffix automaton built for a text; the cache keeping automata alive
    /// for its TTL; a budget share not given back.
    #[tokio::test]
    async fn automata_live_while_a_reader_holds_them() {
        let (logs, units) =
            service_with(AUTOMATA_BUDGET_BYTES, &[("build", b"s3cr3t-value")]).await;
        let unit = &units[0];
        let full = logs.limits.budget.available_permits();
        let cached = logs.load_values(unit).await.unwrap();
        assert_eq!(cached.built(), (false, false), "a launch builds nothing");
        assert_eq!(
            logs.redact_text_for_handle("pod-build", "pull failed: s3cr3t-value")
                .await,
            "pull failed: ***"
        );
        assert_eq!(cached.built(), (false, false), "a cause frees its trie");
        let trie = cached.text_automaton().await.unwrap();
        assert_eq!(cached.built(), (true, false), "a text builds the trie only");
        drop(trie);
        let mut r = logs.stream_redactor(cached.clone()).await.unwrap();
        assert_eq!(cached.built(), (true, true));
        assert!(logs.limits.budget.available_permits() < full);
        assert_eq!(r.push(b"x s3cr3t-value y"), b"x *** y");
        let reloaded = logs.reload_values(unit).await.unwrap();
        assert!(!Arc::ptr_eq(&reloaded, &cached));
        assert!(
            reloaded.shares_automata_with(&cached),
            "same generation, same automata"
        );
        let second = logs.stream_redactor(reloaded.clone()).await.unwrap();
        drop(r);
        assert_eq!(reloaded.built(), (true, true), "a reader still holds them");
        drop(second);
        assert_eq!(
            reloaded.built(),
            (false, false),
            "the last reader freed them"
        );
        assert_eq!(logs.limits.budget.available_permits(), full);
        assert!(
            Arc::ptr_eq(&logs.load_values(unit).await.unwrap(), &reloaded),
            "the cache keeps the values"
        );
    }

    /// A build waits for room in the process budget and goes ahead when a
    /// reader ends; it never reads without its automata. The budget here
    /// fits one unit's automata. Kills: no budget; a budget that lets a
    /// build through unredacted or without a reservation.
    #[tokio::test]
    async fn a_build_waits_for_room_in_the_budget() {
        let (logs, units) =
            service_with(1024, &[("a", b"s3cr3t-value"), ("b", b"other-secret")]).await;
        let a = logs.load_values(&units[0]).await.unwrap();
        let b = logs.load_values(&units[1]).await.unwrap();
        let first = logs.stream_redactor(a).await.unwrap();
        assert_eq!(logs.limits.budget.available_permits(), 0);
        let mut waiting = Box::pin(logs.stream_redactor(b.clone()));
        assert!(
            futures::poll!(&mut waiting).is_pending(),
            "the budget is full"
        );
        assert_eq!(b.built(), (false, false));
        drop(first);
        let mut second = waiting.await.unwrap();
        assert_eq!(second.push(b"x other-secret y"), b"x *** y");
    }

    /// At most [`CONCURRENT_BUILDS`] builds run at once: a build waits for
    /// a slot. Kills: unbounded concurrent builds.
    #[tokio::test]
    async fn a_build_waits_for_a_build_slot() {
        let (logs, units) = service_with(AUTOMATA_BUDGET_BYTES, &[("a", b"s3cr3t-value")]).await;
        let a = logs.load_values(&units[0]).await.unwrap();
        let slots = logs
            .limits
            .builds
            .acquire_many(CONCURRENT_BUILDS as u32)
            .await
            .unwrap();
        let mut waiting = Box::pin(logs.stream_redactor(a.clone()));
        assert!(futures::poll!(&mut waiting).is_pending(), "no build slot");
        drop(slots);
        let mut r = waiting.await.unwrap();
        assert_eq!(r.push(b"s3cr3t-value\n"), b"***\n");
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
