//! Log **source** wiring (ADR-0013): drain a running unit's live stdout/stderr
//! tail into the [`LogService`] pipeline.
//!
//! The pipeline (chunk → gzip → object store + Postgres offset index → live SSE
//! tail + secret redaction) was built and tested independently; what was missing
//! was the *source* feeding [`LogService::append`]. On Kubernetes that source is
//! the executor's log tail ([`Executor::log_stream`], the k8s API log endpoint
//! with `follow: true`) — an **agentless, control-plane-pull** channel, separate
//! from the acked results-egress sidecar (ADR-0042): logs are best-effort, so a
//! failed or dropped tail never fails the run.
//!
//! [`pump_log_stream`] is the pure drain loop (cluster-free, unit-tested);
//! [`LogTailer`] owns the per-fence dedup + task spawning the converged driver
//! calls once per tick for each running step.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use scarab_engine::{AttemptId, Executor, LogChunkMeta, LogChunks, RunId, StepId, StepRun};

use crate::logs::{LogError, LogService};

/// Cool-down before re-tailing a fence whose last attempt errored. Without it, a
/// step whose Pod never produces a log (e.g. stuck in `CreateContainerConfigError`
/// until it is failed) would be re-tailed every driver tick — hammering the k8s
/// API a few times a second. One retry every few seconds is plenty to pick up a
/// Pod that finally starts.
const RETRY_BACKOFF: Duration = Duration::from_secs(3);

/// How long a step Pod may sit in a benign pre-start state (`PodInitializing` /
/// `ContainerCreating`) before the quiet retry loop is itself treated as a
/// problem. Generous, because a cold image pull on a fresh node legitimately
/// takes minutes (c653742) and the step's own timeout is the real deadline. Past
/// this bound, "not ready" almost always means *wedged* — an init container that
/// never completes, a volume that never mounts — and a tail that never starts
/// must stay visible instead of being silent forever.
const NOT_READY_GRACE: Duration = Duration::from_secs(180);

/// Once past [`NOT_READY_GRACE`], re-state a stuck tail at WARN at most this
/// often: present in any log window an operator looks at, without going back to
/// a line every [`RETRY_BACKOFF`].
const STUCK_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Reads of an ended fence before its log is given up (git-bug 1dee2f3). A
/// failed read is retried after the backoff; most failures here mean the
/// container never ran or its Pod is gone, so a few tries are enough.
const FINAL_READ_TRIES: u32 = 3;

/// Errors from draining a log tail. Both arms are best-effort at the call site —
/// the tail is logged and dropped, never failing the run (ADR-0013).
#[derive(Debug, thiserror::Error)]
pub enum LogTailError {
    #[error("executor error: {0}")]
    Exec(#[from] scarab_engine::ExecError),
    #[error("log pipeline error: {0}")]
    Log(#[from] LogError),
}

/// The chunk stored before a log source is stored again from its first
/// byte: the source no longer begins with the part already stored (the
/// kubelet rotated the container's log file, a sidecar or service container
/// restarted, or the secret values changed), so its earlier lines may repeat.
/// Always stored as a chunk of its own, so a read can find it by its length
/// and body (see `current_part`).
pub const LOG_SOURCE_RESTARTED: &[u8] =
    b"\nscarab: the log source restarted; earlier lines may repeat\n";

/// Drain one log source into the pipeline: read chunks until end-of-stream
/// and append them to [`LogService`] under the `{run, step, attempt}` fence,
/// redacted as one stream ([`LogService::redactor`]), so a secret split
/// across two chunks is still replaced. Every byte is appended; this does not
/// compare with what is stored, which the [`LogTailer`]'s reads do. Returns
/// the number of bytes appended (after redaction).
///
/// Empty chunks are skipped (a zero-length read is treated as end-of-stream by
/// the source, but a source that yields an empty-but-not-final chunk shouldn't
/// create a spurious index row).
pub async fn pump_log_stream(
    chunks: Box<dyn LogChunks>,
    logs: &LogService,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
) -> Result<u64, LogTailError> {
    match pump(chunks, logs, run, step, attempt, Vec::new()).await? {
        PumpEnd::Done(p) => Ok(p.appended),
        // An empty stored part has nothing to differ from.
        PumpEnd::Mismatch { .. } | PumpEnd::Unverifiable { .. } => Ok(0),
    }
}

/// What one read of a log source did.
#[derive(Debug, Clone, Copy)]
struct Pumped {
    /// Bytes read from the source, including the part already stored.
    read: u64,
    /// Bytes appended to the pipeline (after redaction).
    appended: u64,
}

/// How one read of a log source ended.
#[derive(Debug)]
enum PumpEnd {
    /// The stream ended cleanly.
    Done(Pumped),
    /// The redacted source differs from the stored part at byte `at` of it,
    /// outside a `***`: the source restarted. Nothing was appended.
    Mismatch { at: u64 },
    /// The redacted source differs from the stored part where the stored
    /// part holds a `***`: this replica does not hold a secret value that
    /// the replica which stored it held. Nothing was appended.
    Unverifiable { at: u64 },
}

/// The **current part** of a stream: every chunk after the newest
/// [`LOG_SOURCE_RESTARTED`] chunk, or the whole stream when there is none.
/// The index rows are read whole; a chunk body is read only when its length
/// is the marker's, newest first. A step that prints the marker text itself
/// can only make the next read store its log again after a marker; it cannot
/// make a read skip output.
async fn current_part(
    logs: &LogService,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
) -> Result<Vec<LogChunkMeta>, LogError> {
    let mut chunks = logs.chunks(run, step, attempt).await?;
    for i in (0..chunks.len()).rev() {
        let c = &chunks[i];
        if c.len == LOG_SOURCE_RESTARTED.len() as u64
            && logs.read_chunk(c).await? == LOG_SOURCE_RESTARTED
        {
            return Ok(chunks.split_off(i + 1));
        }
    }
    Ok(chunks)
}

/// Compares a read's redacted output with the stored part, one stored chunk
/// body in memory at a time.
struct Verify {
    /// Stored chunks not loaded yet.
    pending: VecDeque<LogChunkMeta>,
    /// The loaded chunk body and how far it has matched.
    body: Vec<u8>,
    pos: usize,
    /// Stored bytes matched so far.
    matched: u64,
    /// The last two stored bytes matched, to tell whether a difference lies
    /// inside a `***`.
    recent: Vec<u8>,
}

/// What [`Verify::check`] found.
enum Check {
    /// All of the output matched; stored bytes may be left.
    Matched,
    /// The first `n` bytes of the output matched and the stored part ends
    /// there: the rest is new.
    Passed(usize),
    /// A byte differs at offset `at` of the stored part.
    Differs { at: u64, in_redaction: bool },
}

impl Verify {
    fn new(part: Vec<LogChunkMeta>) -> Self {
        Self {
            pending: part.into(),
            body: Vec::new(),
            pos: 0,
            matched: 0,
            recent: Vec::new(),
        }
    }

    /// Load the next stored chunk when the current one is used up. False when
    /// the stored part is used up.
    async fn load(&mut self, logs: &LogService) -> Result<bool, LogError> {
        while self.pos == self.body.len() {
            match self.pending.pop_front() {
                Some(meta) => {
                    self.body = logs.read_chunk(&meta).await?;
                    self.pos = 0;
                }
                None => return Ok(false),
            }
        }
        Ok(true)
    }

    async fn check(&mut self, logs: &LogService, out: &[u8]) -> Result<Check, LogError> {
        let mut i = 0;
        while i < out.len() {
            if !self.load(logs).await? {
                return Ok(Check::Passed(i));
            }
            let n = (out.len() - i).min(self.body.len() - self.pos);
            let theirs = &self.body[self.pos..self.pos + n];
            if let Some(d) = (0..n).find(|&k| out[i + k] != theirs[k]) {
                self.advance(d);
                let at = self.matched;
                let in_redaction = self.differs_in_redaction(logs).await?;
                return Ok(Check::Differs { at, in_redaction });
            }
            self.advance(n);
            i += n;
        }
        Ok(Check::Matched)
    }

    /// Mark `n` bytes of the loaded body as matched.
    fn advance(&mut self, n: usize) {
        let matched = &self.body[self.pos..self.pos + n];
        self.recent
            .extend_from_slice(&matched[matched.len().saturating_sub(2)..]);
        let excess = self.recent.len().saturating_sub(2);
        self.recent.drain(..excess);
        self.pos += n;
        self.matched += n as u64;
    }

    /// Whether a `***` of the stored part covers the differing byte: one
    /// starting up to two bytes before it, or at it.
    async fn differs_in_redaction(&mut self, logs: &LogService) -> Result<bool, LogError> {
        let mut window = self.recent.clone();
        let before = window.len();
        while window.len() < before + 3 && self.load(logs).await? {
            let take = (before + 3 - window.len()).min(self.body.len() - self.pos);
            window.extend_from_slice(&self.body[self.pos..self.pos + take]);
            self.pos += take;
        }
        Ok((0..=before).any(|s| window.get(s..s + 3) == Some(b"***".as_slice())))
    }

    /// Stored bytes not matched yet, for the debug line of a read that ended
    /// inside the stored part.
    fn left(&self) -> u64 {
        (self.body.len() - self.pos) as u64 + self.pending.iter().map(|c| c.len).sum::<u64>()
    }
}

/// One read of a log source. Every chunk goes through one redactor from the
/// source's first byte. While `part` (the stored part, from
/// [`current_part`]) has not all matched, the redacted output is compared
/// with it and dropped; once it has, the rest is appended. A difference ends
/// the read with nothing appended. On a clean end the redactor's held bytes
/// are emitted like any output; on an error they are dropped (they may be the
/// start of a secret, and the next read sees them again).
///
/// Memory: one source chunk, one stored chunk body, the redactor's held bytes
/// and the stored part's index rows.
async fn pump(
    mut chunks: Box<dyn LogChunks>,
    logs: &LogService,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
    part: Vec<LogChunkMeta>,
) -> Result<PumpEnd, LogTailError> {
    let mut redactor = Some(logs.redactor());
    let mut verify = (!part.is_empty()).then(|| Verify::new(part));
    let mut read = 0u64;
    let mut appended = 0u64;
    while let Some(r) = redactor.as_mut() {
        let out = match chunks.next_chunk().await? {
            Some(chunk) if chunk.is_empty() => continue,
            Some(chunk) => {
                read += chunk.len() as u64;
                r.push(&chunk)
            }
            // A clean end: emit the held bytes; the loop ends after them.
            None => redactor.take().map(|r| r.finish()).unwrap_or_default(),
        };
        let mut new_from = 0;
        if let Some(v) = &mut verify {
            match v.check(logs, &out).await? {
                Check::Matched => new_from = out.len(),
                Check::Passed(n) => new_from = n,
                Check::Differs { at, in_redaction } => {
                    return Ok(if in_redaction {
                        PumpEnd::Unverifiable { at }
                    } else {
                        PumpEnd::Mismatch { at }
                    });
                }
            }
            if new_from < out.len() {
                verify = None;
            }
        }
        let new = &out[new_from..];
        if !new.is_empty() {
            if appended == 0 {
                tracing::info!(run = %run.0, step = %step.0, attempt = %attempt.0, "log tail: receiving output");
            }
            logs.append_redacted(run, step, attempt, new).await?;
            appended += new.len() as u64;
        }
    }
    if let Some(v) = verify.filter(|v| v.left() > 0) {
        // The source ended inside the stored part and matched it so far:
        // every byte read is stored already.
        tracing::debug!(
            run = %run.0, step = %step.0, attempt = %attempt.0, read, left = v.left(),
            "log tail: the log ended inside the part already stored; nothing appended"
        );
    }
    Ok(PumpEnd::Done(Pumped { read, appended }))
}

/// Identifies one log stream: a single attempt of a step. Matches the pipeline's
/// own stream key, so a tail maps one-to-one onto a persisted stream.
type Fence = (String, String, String);

fn fence_of(run: &RunId, step: &StepId, attempt: &AttemptId) -> Fence {
    (run.0.clone(), step.0.clone(), attempt.0.clone())
}

/// The tail-lease TTL (ADR-0051): long enough to survive a slow tick, short
/// enough that a crashed replica's steps are re-tailed within a minute.
const TAIL_LEASE_TTL_MS: i64 = 45_000;

/// Spawns and tracks per-fence log tails. The converged driver calls
/// [`ensure`](LogTailer::ensure) for every running step each tick; the tailer
/// dedups by fence so a step is tailed exactly once per attempt while it runs,
/// and re-arms a fresh tail after a re-armed attempt (a new attempt id is a new
/// fence).
pub struct LogTailer {
    executor: Arc<dyn Executor>,
    logs: Arc<LogService>,
    /// The claim-to-tail lease store (ADR-0051): with `Some`, a fence is
    /// tailed only while THIS replica holds `tail:{run}:{step}:{attempt}` —
    /// deduping ingestion across replicas and distributing the log I/O.
    /// `None` = single-replica mode (in-process dedup only).
    lease: Option<(Arc<dyn scarab_engine::Db>, String)>,
    /// Fences with a tail task currently in flight (guards against double-tailing
    /// across ticks). A tail removes itself here when it ends, so an early
    /// failure (Pod still Pending, log not yet available) is retried next tick.
    active: Arc<Mutex<HashSet<Fence>>>,
    /// Fences whose tail drained to end-of-stream (the followed Pod finished and
    /// the API closed the log). These are complete and must never be re-tailed —
    /// otherwise a step that stays `running` in the store for a few ticks after
    /// its Pod exits gets its whole stdout re-ingested every tick, duplicating
    /// the log N times. Bounded by total step-attempts over the process lifetime.
    drained: Arc<Mutex<HashSet<Fence>>>,
    /// Earliest instant a fence whose last tail errored may be retried — a
    /// per-fence backoff so a Pod with no log yet isn't re-tailed every tick.
    retry_at: Arc<Mutex<HashMap<Fence, Instant>>>,
    /// Fences whose tail keeps failing with a benign pre-start reason, and how
    /// loudly we've reported it. Lets a legitimately-slow start stay quiet while
    /// a Pod stuck in `PodInitializing` forever still surfaces (c653742).
    not_ready: Arc<Mutex<HashMap<Fence, NotReady>>>,
    /// Wait before re-tailing a fence whose last tail errored or read
    /// nothing ([`RETRY_BACKOFF`] unless a test shortens it).
    retry_backoff: Duration,
    /// Fences tailed at least once and not drained yet, with what reopens
    /// them. When the driver stops ensuring one (its step left `Running`),
    /// [`end_pass`](LogTailer::end_pass) reads it once more (git-bug 1dee2f3).
    open: Arc<Mutex<HashMap<Fence, OpenTail>>>,
    /// Fences ensured since the last [`end_pass`](LogTailer::end_pass).
    seen: Mutex<HashSet<Fence>>,
}

/// How to reopen a fence that is not drained yet.
#[derive(Clone)]
struct OpenTail {
    resource: String,
    run: RunId,
    step: StepId,
    attempt: AttemptId,
    source: TailSource,
    /// Failed reads since the fence's unit ended.
    final_tries: u32,
    /// A read of the ended unit has started; later ones wait out the backoff.
    final_started: bool,
}

/// Per-fence bookkeeping for a tail parked in a benign pre-start state: when the
/// fence FIRST reported one, and when we last escalated it to WARN.
#[derive(Clone, Copy)]
struct NotReady {
    since: Instant,
    last_warn: Option<Instant>,
}

impl LogTailer {
    pub fn new(executor: Arc<dyn Executor>, logs: Arc<LogService>) -> Self {
        Self {
            executor,
            logs,
            lease: None,
            active: Arc::new(Mutex::new(HashSet::new())),
            drained: Arc::new(Mutex::new(HashSet::new())),
            retry_at: Arc::new(Mutex::new(HashMap::new())),
            not_ready: Arc::new(Mutex::new(HashMap::new())),
            retry_backoff: RETRY_BACKOFF,
            open: Arc::new(Mutex::new(HashMap::new())),
            seen: Mutex::new(HashSet::new()),
        }
    }

    /// End one driver pass of `ensure*` calls. A fence that is still open but
    /// was not ensured in this pass belongs to a unit that ended (a step that
    /// left `Running`, a retried attempt, a stopped service): its log is read
    /// once more, so a step that ran and finished inside a retry backoff
    /// keeps its output (git-bug 1dee2f3). The first such read does not wait
    /// out a retry backoff left by the live tail. Like every read, it
    /// compares the log with the part the fence has stored already, by this
    /// replica or another, and appends what follows it: output printed after
    /// a live tail failed part way through is stored, and nothing is stored
    /// twice (git-bug de2bd9b; see `drain`).
    /// `complete` is false when the pass did not ensure every unit (a store
    /// error); such a pass only resets the bookkeeping.
    pub fn end_pass(&self, complete: bool) {
        let seen = std::mem::take(&mut *self.seen.lock().unwrap());
        if !complete {
            return;
        }
        let ended: Vec<(Fence, OpenTail)> = self
            .open
            .lock()
            .unwrap()
            .iter()
            .filter(|(fence, _)| !seen.contains(*fence))
            .map(|(fence, tail)| (fence.clone(), tail.clone()))
            .collect();
        for (fence, tail) in ended {
            self.spawn_tail(
                fence,
                tail.resource,
                tail.run,
                tail.step,
                tail.attempt,
                tail.source,
                true,
            );
        }
    }

    /// Wait `backoff` instead of [`RETRY_BACKOFF`] before re-tailing a fence
    /// whose last tail errored or read nothing.
    pub fn with_retry_backoff(mut self, backoff: Duration) -> Self {
        self.retry_backoff = backoff;
        self
    }

    /// Enable the claim-to-tail lease (ADR-0051): required for 2+ replicas.
    pub fn with_lease(mut self, db: Arc<dyn scarab_engine::Db>, owner: impl Into<String>) -> Self {
        self.lease = Some((db, owner.into()));
        self
    }

    /// Ensure a best-effort tail is running for `step`'s current attempt.
    /// Idempotent per fence, so it is safe to call every driver tick. A step with
    /// no current attempt (not launched yet) is a no-op.
    pub fn ensure(&self, step: &StepRun) {
        let Some(attempt) = step.current_attempt() else {
            return;
        };
        let attempt = attempt.id.clone();
        let fence = fence_of(&step.run, &step.step, &attempt);
        let resource = format!("tail:{}:{}:{}", step.run.0, step.step.0, attempt.0);
        self.spawn_tail(
            fence,
            resource,
            step.run.clone(),
            step.step.clone(),
            attempt,
            TailSource::Step(step.clone()),
            false,
        );
    }

    /// Ensure a best-effort tail is running for a **shared service** instance
    /// (ADR-0058 evidence). Keyed on the synthetic `{run, take, name}` stream
    /// (see [`crate::logs::service_stream_key`]) so a service's logs replay/tail
    /// through the exact same pipeline as step logs without a second channel.
    /// Idempotent per instance; call it every tick for each running service.
    pub fn ensure_service(&self, run: &RunId, take: i64, name: &str, handle: &str) {
        let (step, attempt) = crate::logs::service_stream_key(name, take);
        let fence = fence_of(run, &step, &attempt);
        let resource = format!("tail:{}:service:{}:{}", run.0, name, attempt.0);
        self.spawn_tail(
            fence,
            resource,
            run.clone(),
            step,
            attempt,
            TailSource::Service(scarab_engine::ports::ExecHandle(handle.to_string())),
            false,
        );
    }

    /// Ensure a best-effort tail is running for the `index`-th **sidecar service**
    /// of `step` (ADR-0058 evidence): the `service-{index}` container co-located in
    /// the step's Pod. Keyed on the synthetic `{step}::service-{index}` stream (see
    /// [`crate::logs::sidecar_stream_key`]) under the step's REAL current attempt,
    /// so it replays/tails through the exact same pipeline as step logs and a
    /// per-attempt read scopes it like the step's own. Idempotent per fence; a step
    /// with no current attempt (not launched yet) is a no-op. Call it every tick for
    /// each running step that declares sidecars.
    pub fn ensure_sidecar(&self, step: &StepRun, index: usize) {
        let Some(attempt) = step.current_attempt() else {
            return;
        };
        let attempt = attempt.id.clone();
        let syn_step = crate::logs::sidecar_stream_key(&step.step, index);
        let fence = fence_of(&step.run, &syn_step, &attempt);
        let resource = format!(
            "tail:{}:{}:sidecar-{index}:{}",
            step.run.0, step.step.0, attempt.0
        );
        self.spawn_tail(
            fence,
            resource,
            step.run.clone(),
            syn_step,
            attempt,
            TailSource::Sidecar {
                step: step.clone(),
                index,
            },
            false,
        );
    }

    /// The shared spawn/lease/backoff machinery behind [`ensure`](Self::ensure)
    /// and [`ensure_service`](Self::ensure_service): dedup by fence, claim the
    /// per-fence tail lease (ADR-0051), drain the `source` into the pipeline under
    /// `{run, step, attempt}`, then release the fence. `source` selects the
    /// executor log endpoint (step Pod vs. service Pod). `final_read` is the
    /// read of a fence whose unit has ended (see [`end_pass`](Self::end_pass)).
    #[allow(clippy::too_many_arguments)]
    fn spawn_tail(
        &self,
        fence: Fence,
        resource: String,
        run: RunId,
        step_id: StepId,
        attempt: AttemptId,
        source: TailSource,
        final_read: bool,
    ) {
        // Already fully drained — the Pod's log is complete; never re-tail it.
        if self.drained.lock().unwrap().contains(&fence) {
            return;
        }
        if !final_read {
            self.seen.lock().unwrap().insert(fence.clone());
            self.open
                .lock()
                .unwrap()
                .entry(fence.clone())
                .or_insert_with(|| OpenTail {
                    resource: resource.clone(),
                    run: run.clone(),
                    step: step_id.clone(),
                    attempt: attempt.clone(),
                    source: source.clone(),
                    final_tries: 0,
                    final_started: false,
                });
        }
        // The first read of an ended unit skips the backoff: the unit has
        // ended, so there is no Pod to hammer, and a live viewer's stream ends
        // soon after the Run settles.
        let first_final = final_read
            && self
                .open
                .lock()
                .unwrap()
                .get(&fence)
                .is_some_and(|t| !t.final_started);
        // Backing off after a recent error — don't hammer a Pod with no log yet.
        if !first_final {
            if let Some(at) = self.retry_at.lock().unwrap().get(&fence) {
                if Instant::now() < *at {
                    return;
                }
            }
        }
        // Claim the fence; bail if a tail is already in flight for it.
        if !self.active.lock().unwrap().insert(fence.clone()) {
            return;
        }
        if first_final {
            if let Some(t) = self.open.lock().unwrap().get_mut(&fence) {
                t.final_started = true;
            }
        }

        let executor = self.executor.clone();
        let logs = self.logs.clone();
        let active = self.active.clone();
        let drained = self.drained.clone();
        let retry_at = self.retry_at.clone();
        let not_ready = self.not_ready.clone();
        let lease = self.lease.clone();
        let retry_backoff = self.retry_backoff;
        let open = self.open.clone();
        // The fence's log is complete (or given up): never read it again.
        let close = {
            let drained = drained.clone();
            let retry_at = retry_at.clone();
            let not_ready = not_ready.clone();
            let open = open.clone();
            move |fence: &Fence| {
                drained.lock().unwrap().insert(fence.clone());
                retry_at.lock().unwrap().remove(fence);
                not_ready.lock().unwrap().remove(fence);
                open.lock().unwrap().remove(fence);
            }
        };

        tokio::spawn(async move {
            // Claim-to-tail (ADR-0051): only the lease holder tails this
            // fence; everyone else backs off and re-checks next tick — when
            // the holder's lease expires (crash), a peer takes over here.
            let mut renewer: Option<tokio::task::JoinHandle<()>> = None;
            if let Some((db, owner)) = &lease {
                match db.lease(&resource, owner, TAIL_LEASE_TTL_MS).await {
                    Ok(l) if &l.owner == owner => {
                        // Ours: renew in the background while the drain runs.
                        let db = db.clone();
                        let owner = owner.clone();
                        renewer = Some(tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    (TAIL_LEASE_TTL_MS / 3) as u64,
                                ))
                                .await;
                                if db
                                    .lease(&resource, &owner, TAIL_LEASE_TTL_MS)
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }));
                    }
                    _ => {
                        // Another replica tails it (or the store hiccuped):
                        // back off, retry the claim next backoff window.
                        retry_at
                            .lock()
                            .unwrap()
                            .insert(fence.clone(), Instant::now() + retry_backoff);
                        active.lock().unwrap().remove(&fence);
                        return;
                    }
                }
            }
            if final_read {
                // The unit has ended: one read of what it printed after the
                // part already stored, if any.
                let result = drain(&*executor, &logs, &source, &run, &step_id, &attempt).await;
                if let Some(r) = renewer {
                    r.abort();
                }
                match result {
                    Ok(pumped) => {
                        if let Some(bytes) = pumped.map(|p| p.appended).filter(|b| *b > 0) {
                            tracing::info!(run = %run.0, step = %step_id.0, attempt = %attempt.0, bytes, "log tail: read the log of an ended unit");
                        }
                        close(&fence);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let tries = {
                            let mut open = open.lock().unwrap();
                            match open.get_mut(&fence) {
                                Some(t) => {
                                    t.final_tries += 1;
                                    t.final_tries
                                }
                                None => FINAL_READ_TRIES,
                            }
                        };
                        if tries < FINAL_READ_TRIES {
                            tracing::debug!(run = %run.0, step = %step_id.0, error = %msg, "log tail: reading an ended unit failed; retrying");
                            retry_at
                                .lock()
                                .unwrap()
                                .insert(fence.clone(), Instant::now() + retry_backoff);
                        } else {
                            if never_ran_or_gone(&msg) {
                                tracing::debug!(run = %run.0, step = %step_id.0, error = %msg, "log tail: an ended unit has no log to read");
                            } else {
                                tracing::warn!(run = %run.0, step = %step_id.0, attempt = %attempt.0, error = %msg, "log tail: could not read the log of an ended unit; its output is not stored");
                            }
                            close(&fence);
                        }
                    }
                }
                active.lock().unwrap().remove(&fence);
                return;
            }
            let result = drain(&*executor, &logs, &source, &run, &step_id, &attempt).await;
            if let Some(r) = renewer {
                r.abort();
            }
            match result {
                // The stream closed cleanly having read nothing. That is not a
                // complete log (git-bug 1dee2f3): Kubernetes answers a log
                // request for a Pod with no node yet (one waiting for an
                // autoscaler to add a node) with an empty stream, not an
                // error, so marking it drained lost every line the step
                // printed later. Retry after the backoff, like a Pod that is
                // not ready. A follow read of a running container stays open,
                // so a silent step costs one empty read per backoff only
                // between its exit and leaving `Running`; then `end_pass`
                // reads it once more and closes the fence. A read that only
                // re-read the stored part did read something: the stream
                // closed because the container ended.
                Ok(Some(Pumped { read: 0, .. })) => {
                    tracing::debug!(run = %run.0, step = %step_id.0, "log tail: the stream ended with no output; retrying");
                    retry_at
                        .lock()
                        .unwrap()
                        .insert(fence.clone(), Instant::now() + retry_backoff);
                }
                // Stream closed cleanly after output, or the executor has no
                // log source: we have the step's complete log. Mark the fence
                // drained so no later tick re-ingests it (dedup fix).
                Ok(pumped) => {
                    if let Some(bytes) = pumped.map(|p| p.appended) {
                        tracing::info!(run = %run.0, step = %step_id.0, attempt = %attempt.0, bytes, "log tail: ended");
                    }
                    close(&fence);
                }
                // Best-effort: a lost tail never fails the run (ADR-0013). The
                // common benign case is a step Pod that hasn't started its
                // container yet (Pending / PodInitializing / ContainerCreating) —
                // an expected pre-start state, not a failure. On a cold image
                // pull that can last minutes, so log it at debug and back off
                // quietly; only WARN on a genuine tail error, or on a pre-start
                // state that has outlasted NOT_READY_GRACE (a tail that never
                // starts must still be visible). Either way, back off before
                // retrying so we don't hammer the API each tick. What the
                // tail appended before it failed is stored; the retry
                // compares with it and appends what follows (git-bug
                // de2bd9b).
                Err(e) => {
                    let msg = e.to_string();
                    let now = Instant::now();
                    let (class, stuck_for) = {
                        let mut nr = not_ready.lock().unwrap();
                        if is_pod_not_ready(&msg) {
                            let entry = nr.entry(fence.clone()).or_insert(NotReady {
                                since: now,
                                last_warn: None,
                            });
                            let stuck_for = now.duration_since(entry.since);
                            let class = classify_tail_error(
                                &msg,
                                stuck_for,
                                entry.last_warn.map(|w| now.duration_since(w)),
                            );
                            if class == TailErrorClass::StuckNotReady {
                                entry.last_warn = Some(now);
                            }
                            (class, stuck_for)
                        } else {
                            // A different (real) failure supersedes any pre-start
                            // streak — start clean if it goes back to waiting.
                            nr.remove(&fence);
                            (TailErrorClass::Failed, Duration::ZERO)
                        }
                    };
                    match class {
                        TailErrorClass::Quiet => {
                            tracing::debug!(run = %run.0, step = %step_id.0, error = %msg, "log tail: step Pod not ready yet, backing off");
                        }
                        TailErrorClass::StuckNotReady => {
                            tracing::warn!(
                                run = %run.0, step = %step_id.0,
                                not_ready_for_s = stuck_for.as_secs(), error = %msg,
                                "log tail: step Pod still has not started its container — \
                                 no logs will appear; check its init containers / image pull"
                            );
                        }
                        TailErrorClass::Failed => {
                            tracing::warn!(run = %run.0, step = %step_id.0, error = %msg, "log tail ended with error");
                        }
                    }
                    retry_at
                        .lock()
                        .unwrap()
                        .insert(fence.clone(), Instant::now() + retry_backoff);
                }
            }
            active.lock().unwrap().remove(&fence);
        });
    }
}

/// How loudly a log-tail error should be reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailErrorClass {
    /// Expected and unremarkable — the container isn't up yet and we're still
    /// inside the grace window (or we warned about it very recently). Debug.
    Quiet,
    /// A pre-start state that has outlasted [`NOT_READY_GRACE`]: the Pod is very
    /// likely wedged and this step's logs will never arrive. Warn.
    StuckNotReady,
    /// A genuine tail failure — RBAC, connectivity, `ImagePullBackOff`. Warn.
    Failed,
}

/// Decide how loudly to report a log-tail error (c653742). Pure, so the policy is
/// unit-testable without asserting on log output.
///
/// `not_ready_for` is how long this fence has been reporting a benign pre-start
/// state; `since_last_warn` how long since we last escalated it to WARN (`None` =
/// never). The rate limit is what keeps a wedged Pod visible without restoring
/// the every-[`RETRY_BACKOFF`] flood the ticket was about.
fn classify_tail_error(
    err: &str,
    not_ready_for: Duration,
    since_last_warn: Option<Duration>,
) -> TailErrorClass {
    if !is_pod_not_ready(err) {
        return TailErrorClass::Failed;
    }
    // A Pod waiting for a node is not wedged: the executor's unschedulable
    // grace fails the step if no node comes, and the infra observer says
    // why it waits (git-bug 1dee2f3).
    if err.contains("PodScheduling") || not_ready_for < NOT_READY_GRACE {
        return TailErrorClass::Quiet;
    }
    match since_last_warn {
        None => TailErrorClass::StuckNotReady,
        Some(d) if d >= STUCK_WARN_INTERVAL => TailErrorClass::StuckNotReady,
        Some(_) => TailErrorClass::Quiet,
    }
}

/// True when a log-tail error just means the step Pod hasn't started its
/// container yet — an expected pre-start state the tail should retry quietly,
/// not a genuine failure. We match on the specific benign container-waiting
/// *reasons* (PodInitializing / ContainerCreating), NOT the generic "is waiting
/// to start:" prefix — that prefix also carries the reasons we DO want to warn
/// about (ImagePullBackOff, ErrImagePull, CrashLoopBackOff, …).
fn is_pod_not_ready(err: &str) -> bool {
    // `PodScheduling` is the k8s executor's own reason for a Pod with no
    // node yet (git-bug 1dee2f3), such as one waiting for an autoscaler.
    const NOT_READY: [&str; 3] = ["PodInitializing", "ContainerCreating", "PodScheduling"];
    NOT_READY.iter().any(|m| err.contains(m))
}

/// A read of an ended unit failed in a way that means there was nothing to
/// read: its container never started, or its Pod is gone (deleted on cancel).
fn never_ran_or_gone(err: &str) -> bool {
    is_pod_not_ready(err) || err.contains("is waiting to start") || err.contains("NotFound")
}

/// The unit whose live log tail is being drained — a step Pod or a shared-service
/// Pod (ADR-0058). Selects which executor log endpoint the drain opens; both pump
/// into the same pipeline under the fence's `{run, step, attempt}` stream key.
#[derive(Clone)]
enum TailSource {
    Step(StepRun),
    Service(scarab_engine::ports::ExecHandle),
    /// The `index`-th sidecar service of `step` (ADR-0058): a `service-{index}`
    /// container in the step's OWN Pod, tailed distinctly from the main step. The
    /// storage `{run, step, attempt}` keys the drive loop passes are already the
    /// synthetic sidecar id + the step's real attempt (see [`LogTailer::ensure_sidecar`]).
    Sidecar {
        step: StepRun,
        index: usize,
    },
}

/// Open the executor's log source for `source`. `Ok(None)`: the backend has
/// no log source (e.g. the local/dev executor).
async fn open_source(
    executor: &dyn Executor,
    source: &TailSource,
) -> Result<Option<Box<dyn LogChunks>>, LogTailError> {
    Ok(match source {
        TailSource::Step(step_run) => executor.log_stream(step_run).await?,
        TailSource::Service(handle) => executor.service_log_stream(handle).await?,
        TailSource::Sidecar { step, index } => {
            // The co-located `service-{index}` container in the step's own Pod.
            let container = format!("service-{index}");
            executor.sidecar_log_stream(step, &container).await?
        }
    })
}

/// Register with `logs` the secret values of the unit a tail reads
/// ([`Executor::log_redactions`]), so a replica that did not launch the unit
/// (a lease takeover, a server roll while it runs) redacts its log like the
/// one that did. A registry dockerconfigjson also registers each credential
/// in it, as the launch does.
async fn register_unit_secrets(
    executor: &dyn Executor,
    logs: &LogService,
    source: &TailSource,
) -> Result<(), LogTailError> {
    let step = match source {
        TailSource::Step(step) | TailSource::Sidecar { step, .. } => step,
        TailSource::Service(_) => return Ok(()),
    };
    for value in executor.log_redactions(step).await? {
        logs.register_secret(&value);
        if let Ok(text) = std::str::from_utf8(&value) {
            for v in crate::forge_token::registry_secret_values(Some(text), None) {
                logs.register_secret(v.as_bytes());
            }
        }
    }
    Ok(())
}

/// Read the log of the unit `source` into the pipeline, after the part
/// already stored (git-bug de2bd9b).
///
/// The Kubernetes log API takes no byte offset, so a read starts at the
/// container log's first byte. The read is redacted from that byte and
/// compared with the stream's **current part** ([`current_part`]); what
/// follows it is appended. A source that does not begin with the current part
/// (a rotated log file, a restarted container, a change in the secret values)
/// gets a WARN, a [`LOG_SOURCE_RESTARTED`] chunk, and one more read whose
/// every byte is appended. If that read fails, the marker stays: the next
/// read compares with the part after it, so a source is never stored twice
/// and a failing tail does not grow the log on every retry.
///
/// A difference where the stored part holds a `***` means this replica lacks
/// a secret value that the one which stored it held (the unit's Secret is
/// gone: an ended unit read after its attempt settled). Nothing is appended
/// and the fence is closed: storing the log again would store that value in
/// clear.
///
/// The unit's secret values are registered first ([`register_unit_secrets`]);
/// failing that fails the read, before anything is read.
async fn drain(
    executor: &dyn Executor,
    logs: &LogService,
    source: &TailSource,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
) -> Result<Option<Pumped>, LogTailError> {
    register_unit_secrets(executor, logs, source).await?;
    let Some(chunks) = open_source(executor, source).await? else {
        return Ok(None);
    };
    let part = current_part(logs, run, step, attempt).await?;
    let stored: u64 = part.iter().map(|c| c.len).sum();
    match pump(chunks, logs, run, step, attempt, part).await? {
        PumpEnd::Done(pumped) => Ok(Some(pumped)),
        PumpEnd::Unverifiable { at } => {
            tracing::warn!(
                run = %run.0, step = %step.0, attempt = %attempt.0, at, stored,
                "log tail: the log differs from the part already stored where that part \
                 holds a redacted value, and this server does not hold the value; \
                 later output of this log is not stored"
            );
            // As for a backend with no log source: the fence is closed.
            Ok(None)
        }
        PumpEnd::Mismatch { at } => {
            tracing::warn!(
                run = %run.0, step = %step.0, attempt = %attempt.0, at, stored,
                "log tail: the log does not begin with the part already stored (rotated \
                 log file, restarted container, or a redaction change); storing it again \
                 after a marker"
            );
            logs.append_redacted(run, step, attempt, LOG_SOURCE_RESTARTED)
                .await?;
            let Some(chunks) = open_source(executor, source).await? else {
                return Ok(None);
            };
            match pump(chunks, logs, run, step, attempt, Vec::new()).await? {
                PumpEnd::Done(pumped) => Ok(Some(pumped)),
                // An empty stored part has nothing to differ from.
                PumpEnd::Mismatch { .. } | PumpEnd::Unverifiable { .. } => Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        classify_tail_error, is_pod_not_ready, TailErrorClass, NOT_READY_GRACE, STUCK_WARN_INTERVAL,
    };

    /// The real dogfood message: a tail against a Pod still initializing.
    const INITIALIZING: &str = "executor error: exec error: ApiError: container \"step\" in pod \
                                \"scarab-check-a1-7893ca80\" is waiting to start: PodInitializing: \
                                BadRequest";

    #[test]
    fn pre_start_states_are_quiet() {
        assert!(is_pod_not_ready(INITIALIZING));
        assert!(is_pod_not_ready(
            "container \"step\" is waiting to start: ContainerCreating"
        ));
    }

    #[test]
    fn genuine_errors_still_warn() {
        assert!(!is_pod_not_ready(
            "ApiError: pods \"x\" is forbidden: cannot get pods/log"
        ));
        assert!(!is_pod_not_ready("connection refused"));
        // A failing image pull is a real problem, not a pre-start state.
        assert!(!is_pod_not_ready(
            "container \"step\" is waiting to start: ImagePullBackOff"
        ));
    }

    /// c653742: a normal cold start (image pull, init containers) must not warn —
    /// that is the whole spam complaint.
    #[test]
    fn a_slow_start_stays_quiet_inside_the_grace_window() {
        assert_eq!(
            classify_tail_error(INITIALIZING, Duration::ZERO, None),
            TailErrorClass::Quiet
        );
        assert_eq!(
            classify_tail_error(INITIALIZING, NOT_READY_GRACE - Duration::from_secs(1), None),
            TailErrorClass::Quiet
        );
    }

    /// …but the warning is not simply deleted: a tail that NEVER starts has to
    /// surface, so past the grace window the pre-start state escalates to WARN.
    /// git-bug 1dee2f3: a Pod with no node yet (waiting for an autoscaler)
    /// is retried quietly however long it waits; its own grace fails it.
    #[test]
    fn a_pod_waiting_for_a_node_stays_quiet() {
        const SCHEDULING: &str = "executor error: exec error: container \"step\" in pod \
                                  \"scarab-plan-a1-1\" is waiting to start: PodScheduling: the \
                                  Pod has no node yet";
        assert!(is_pod_not_ready(SCHEDULING));
        assert_eq!(
            classify_tail_error(SCHEDULING, NOT_READY_GRACE * 5, None),
            TailErrorClass::Quiet
        );
    }

    #[test]
    fn a_pod_stuck_past_the_grace_window_warns() {
        assert_eq!(
            classify_tail_error(INITIALIZING, NOT_READY_GRACE, None),
            TailErrorClass::StuckNotReady
        );
    }

    /// The escalation is rate-limited, so a wedged Pod is one line a minute — not
    /// one line every RETRY_BACKOFF, which is the bug we're fixing.
    #[test]
    fn a_stuck_pod_is_warned_about_periodically_not_every_retry() {
        let stuck = NOT_READY_GRACE + Duration::from_secs(600);
        assert_eq!(
            classify_tail_error(INITIALIZING, stuck, Some(Duration::from_secs(3))),
            TailErrorClass::Quiet,
            "just warned — the next retry stays quiet"
        );
        assert_eq!(
            classify_tail_error(INITIALIZING, stuck, Some(STUCK_WARN_INTERVAL)),
            TailErrorClass::StuckNotReady,
            "still stuck a minute later — restate it so it stays visible"
        );
    }

    /// A real failure is loud immediately, however long it has been waiting; the
    /// grace window is only for the benign pre-start reasons.
    #[test]
    fn a_genuine_failure_warns_immediately() {
        assert_eq!(
            classify_tail_error(
                "ApiError: pods \"x\" is forbidden: cannot get pods/log",
                Duration::ZERO,
                None
            ),
            TailErrorClass::Failed
        );
        assert_eq!(
            classify_tail_error(
                "container \"step\" is waiting to start: ImagePullBackOff",
                Duration::ZERO,
                Some(Duration::from_secs(1))
            ),
            TailErrorClass::Failed
        );
    }
}
