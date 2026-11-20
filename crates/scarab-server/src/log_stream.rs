//! Lazy, batched, resumable log streams (ADR-0013) — the one replay + live-tail
//! machine behind the run-wide, per-step and shared-service SSE routes.
//!
//! - **Lazy.** The index is read up front; bodies are fetched and gunzipped
//!   batch by batch as the stream is polled, the chunks of one batch fetched
//!   ahead in order (up to [`PREFETCH_CHUNKS`](crate::logs::PREFETCH_CHUNKS)
//!   at once). At most one batch is in memory.
//! - **Batches.** A `data` event carries consecutive whole chunks of ONE attempt,
//!   up to [`BATCH_BYTES`]; a bigger chunk goes alone. A batch ends where the
//!   bytes end on a whole UTF-8 character: if a chunk boundary falls inside one,
//!   the batch takes the next chunk too, but only one past [`BATCH_BYTES`]; a
//!   batch still cut inside a character after that ends at its last whole
//!   boundary, or goes out with the cut bytes replaced. So one batch holds at
//!   most [`BATCH_BYTES`] plus two chunks. A running attempt's newest chunk that
//!   ends inside a character waits for the next poll, for at most
//!   [`HOLD_PASSES`] passes; then it goes out with the cut bytes replaced.
//! - **Cursor.** Every `data` event has `id: <attempt_id>:<next_seq>`, the
//!   position after it. A stream opened [`Start::From`] a cursor skips the
//!   attempts before that one and restarts it at `next_seq`.
//! - **Line ends.** `\r\n` goes out as `\n`, also when the `\r` ends one
//!   batch and the `\n` starts the next: the stream sends the `\r` and drops
//!   the `\n`. axum writes each `\r` and `\n` of a `data` event as a line
//!   break of its own, so a `\r\n` would reach the reader as two. A lone `\r`
//!   reaches the reader as SSE delivers it, a line break. The raw download
//!   keeps the stored bytes.
//! - **Tail.** [`Start::Tail`] starts at the chunk that holds the `tail`-th
//!   byte from the end, so at least `min(tail, total)` bytes are replayed and
//!   at most one chunk more than `tail`, announced by one `truncated` event
//!   first. Its `total_bytes` is the stored length when the stream opened.
//! - **Live.** Polls the durable index every 500 ms (ADR-0051), so any replica
//!   serves the tail; a poll reads only the index rows from the last one sent
//!   on; a keepalive comment when a poll finds nothing; ends one
//!   poll after the run settles or its row is gone, or after a pinned attempt
//!   is no longer latest. An attempt that is not running stops being polled
//!   once it has sent everything and nothing new came for [`SETTLE`] (the read
//!   of an ended unit's log can land a few seconds after it ends).
//! - **Still being stored.** An attempt that is not running and whose tail
//!   lease is live
//!   ([`tail_lease_resource`]) is still being read into the store: a log
//!   tail can fall behind a step that prints fast and store its end long
//!   after the step ended. Such a source is followed like a running one, and
//!   the stream does not end while one is, for at most [`STORE_WAIT`] after
//!   it would have ended; a stream of a settled run opens live while one is.
//!   A `storing` event says when that starts (`{"storing":true}`) and when it
//!   stops (`{"storing":false}`), so a reader can tell a log still being
//!   stored from one with no output; `{"storing":true,"wait_over":true}`
//!   says the stream ends after [`STORE_WAIT`] with the log still being
//!   stored. The running attempt never counts: its tail holds its lease for
//!   as long as it runs.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{Event, Sse};
use futures::stream::{self, BoxStream, StreamExt};

use scarab_engine::{AttemptId, AttemptOutcome, Db, LogChunkMeta, RunId, StepId};
use tokio::time::Instant;

use crate::log_tail::tail_lease_resource;
use crate::logs::{LogError, LogService};
use crate::ApiError;

/// The most bytes one `data` event carries (a single bigger chunk goes alone).
pub const BATCH_BYTES: u64 = 256 * 1024;
/// The largest `?tail=` honoured; larger values are clamped to this.
pub const MAX_TAIL: u64 = 64 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(500);
/// How many passes a running attempt's newest chunk that ends inside a
/// character is held back, waiting for the rest of the character.
pub const HOLD_PASSES: u32 = 2;
/// How long an attempt that is not running must have sent everything, with no
/// new chunk, before a live stream stops reading its index.
pub const SETTLE: Duration = Duration::from_secs(30);
/// How long a stream that would end (its run settled, or its pinned attempt
/// is no longer latest) still waits for a source whose log is still being
/// stored.
pub const STORE_WAIT: Duration = Duration::from_secs(600);

/// A resume position: `<attempt_id>:<next_seq>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub attempt: AttemptId,
    pub next_seq: u64,
}

impl Cursor {
    /// Parse `<attempt_id>:<next_seq>`. The attempt id may itself hold `:`;
    /// the seq is after the last one.
    pub fn parse(s: &str) -> Option<Self> {
        let (attempt, seq) = s.rsplit_once(':')?;
        if attempt.is_empty() {
            return None;
        }
        Some(Self {
            attempt: AttemptId(attempt.to_string()),
            next_seq: seq.parse().ok()?,
        })
    }

    fn format(attempt: &AttemptId, next_seq: u64) -> String {
        format!("{}:{next_seq}", attempt.0)
    }
}

/// Where a stream's sources come from, re-resolved on every live poll.
pub enum Scope {
    /// Every attempt of every step of the run, in step order.
    Run,
    /// One step's attempts in order, read under `read_step` (the step, or a
    /// sidecar's synthetic key). `pinned` restricts it to one attempt.
    Step {
        step: StepId,
        read_step: StepId,
        pinned: Option<AttemptId>,
    },
    /// One fixed stream (a shared-service instance).
    Fixed { step: StepId, attempt: AttemptId },
}

/// Where the replay starts.
pub enum Start {
    /// Everything committed.
    Full,
    /// Resume after a cursor the client already holds.
    From(Cursor),
    /// From the chunk that holds this many bytes before the end (clamped to
    /// `[1, MAX_TAIL]`): at least that many bytes, or the whole log.
    Tail(u64),
}

/// One thing the stream emits, before it becomes an SSE frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Truncated {
        skipped_bytes: u64,
        total_bytes: u64,
    },
    Data {
        id: String,
        text: String,
    },
    /// Whether a source of the stream that is not running is still being
    /// stored; sent when it changes, starting from `false`.
    Storing(bool),
    /// The stream ends while a source is still being stored: it waited
    /// [`STORE_WAIT`] for it.
    StoreWaitOver,
    Keepalive,
    Error(String),
}

impl Item {
    fn into_event(self) -> Event {
        match self {
            Item::Truncated {
                skipped_bytes,
                total_bytes,
            } => Event::default().event("truncated").data(
                serde_json::json!({
                    "skipped_bytes": skipped_bytes,
                    "total_bytes": total_bytes,
                })
                .to_string(),
            ),
            Item::Data { id, text } => {
                // axum panics on an id holding CR, LF or NUL; attempt ids never
                // do, but a frame without an id beats a panic.
                let ev = if id.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
                    Event::default()
                } else {
                    Event::default().id(id)
                };
                ev.data(text)
            }
            Item::Storing(storing) => Event::default()
                .event("storing")
                .data(serde_json::json!({ "storing": storing }).to_string()),
            Item::StoreWaitOver => Event::default()
                .event("storing")
                .data(serde_json::json!({ "storing": true, "wait_over": true }).to_string()),
            Item::Keepalive => Event::default().comment("keepalive"),
            Item::Error(msg) => Event::default().event("error").data(msg),
        }
    }
}

struct Source {
    step: StepId,
    attempt: AttemptId,
    /// The latest attempt of its step, still running: it may still get chunks.
    may_grow: bool,
    /// Its tail lease was live at the last check: its log is still being
    /// stored, so it may still get chunks too.
    storing: bool,
}

impl Source {
    fn grows(&self) -> bool {
        self.may_grow || self.storing
    }
}

type Key = (String, String);

fn key(step: &StepId, attempt: &AttemptId) -> Key {
    (step.0.clone(), attempt.0.clone())
}

/// Sentinel progress for a source the cursor or the tail cut skips entirely.
const SKIPPED: u64 = u64::MAX;

/// The replay + live-tail state machine. Drive it with [`LogStream::next`].
pub struct LogStream {
    logs: Arc<LogService>,
    db: Arc<dyn Db>,
    run: RunId,
    scope: Scope,
    sources: Vec<Source>,
    /// Next seq to send, per `(read step, attempt)`.
    progress: HashMap<Key, u64>,
    /// Index into `sources` this pass is draining, and its index once loaded.
    cur: usize,
    index: Option<Vec<LogChunkMeta>>,
    /// Drop leading UTF-8 continuation bytes from the next batch sent for this
    /// source (a tail cut can land inside a character).
    strip_lead: Option<Key>,
    /// Per source: a held-back newest chunk ending inside a character, as
    /// (index length when held, the pass it was first held in). Counted in
    /// passes, not calls: one pass can read a source more than once.
    held: HashMap<Key, (u64, u64)>,
    /// Passes started so far: 0 is the opening replay, each live poll adds 1.
    pass: u64,
    /// Per source that is not running: since when it has had nothing to send.
    quiet_since: HashMap<Key, Instant>,
    /// Chunk-index reads so far.
    index_reads: u64,
    /// Per `(read step, attempt)`: the last byte sent was `\r`, so a `\n`
    /// that starts the next batch is the rest of a `\r\n` and is dropped.
    /// Missing for a source this stream has not sent yet.
    ends_cr: HashMap<Key, bool>,
    pending: VecDeque<Item>,
    /// Keep polling once the current pass is drained.
    live: bool,
    /// The current pass is the last one.
    last_pass: bool,
    /// The current pass is a live poll (not the opening replay).
    polled: bool,
    sent_in_pass: bool,
    done: bool,
    /// Since when the stream would have ended but for a source still being
    /// stored.
    ending_since: Option<Instant>,
    /// What the last `storing` event said (none sent: `false`).
    storing_sent: bool,
}

impl LogStream {
    /// Resolve the sources and plan the start. `Err(BadRequest)` when a
    /// cursor names an attempt outside the selection.
    pub async fn open(
        logs: Arc<LogService>,
        db: Arc<dyn Db>,
        run: RunId,
        scope: Scope,
        start: Start,
        live: bool,
    ) -> Result<Self, ApiError> {
        let sources = resolve(db.as_ref(), &run, &scope).await?.0;
        let mut s = Self {
            logs,
            db,
            run,
            scope,
            sources,
            progress: HashMap::new(),
            cur: 0,
            index: None,
            strip_lead: None,
            held: HashMap::new(),
            pass: 0,
            quiet_since: HashMap::new(),
            index_reads: 0,
            ends_cr: HashMap::new(),
            pending: VecDeque::new(),
            live,
            last_pass: false,
            polled: false,
            sent_in_pass: false,
            done: false,
            ending_since: None,
            storing_sent: false,
        };
        // A settled run's log may still be being stored: follow it until it
        // is, as a running one is.
        if s.check_storing().await && !s.live {
            s.live = true;
        }
        match start {
            Start::Full => {}
            Start::From(c) => {
                let at = s
                    .sources
                    .iter()
                    .position(|src| src.attempt == c.attempt)
                    .ok_or_else(|| {
                        ApiError::BadRequest(format!(
                            "cursor names attempt {:?}, which is not in this stream",
                            c.attempt.0
                        ))
                    })?;
                for src in &s.sources[..at] {
                    s.progress.insert(key(&src.step, &src.attempt), SKIPPED);
                }
                let src = &s.sources[at];
                s.progress.insert(key(&src.step, &src.attempt), c.next_seq);
            }
            Start::Tail(tail) => s.plan_tail(tail.clamp(1, MAX_TAIL)).await?,
        }
        Ok(s)
    }

    async fn plan_tail(&mut self, tail: u64) -> Result<(), LogError> {
        let mut indexes = Vec::with_capacity(self.sources.len());
        for src in &self.sources {
            indexes.push(self.logs.chunks(&self.run, &src.step, &src.attempt).await?);
            self.index_reads += 1;
        }
        let total: u64 = indexes.iter().flatten().map(|c| c.len).sum();
        if total <= tail {
            return Ok(());
        }
        // Walk back from the end to the chunk that holds the `tail`-th byte
        // from the end, and start at it: at least `tail` bytes are sent, and
        // less than one chunk more. Starting after it instead sent up to a
        // chunk less than `tail`, and a log stored in big chunks showed much
        // less than the notice's tail.
        let mut kept = 0u64;
        let mut start: Option<(usize, u64)> = None;
        'walk: for (i, chunks) in indexes.iter().enumerate().rev() {
            for c in chunks.iter().rev() {
                if kept >= tail {
                    break 'walk;
                }
                kept += c.len;
                start = Some((i, c.seq));
            }
        }
        let Some((at, seq)) = start else {
            return Ok(());
        };
        if kept >= total {
            return Ok(());
        }
        for src in &self.sources[..at] {
            self.progress.insert(key(&src.step, &src.attempt), SKIPPED);
        }
        let src = &self.sources[at];
        self.progress.insert(key(&src.step, &src.attempt), seq);
        // The reader never saw the byte before the cut: keep a leading `\n`
        // rather than read the chunk before it to decide.
        self.ends_cr.insert(key(&src.step, &src.attempt), false);
        self.pending.push_back(Item::Truncated {
            skipped_bytes: total - kept,
            total_bytes: total,
        });
        self.strip_lead = Some(key(&src.step, &src.attempt));
        Ok(())
    }

    /// The next item, or `None` at the end. With `poll == false` it stops
    /// (returns `None` without ending) where it would wait for a live poll.
    pub async fn next(&mut self, poll: bool) -> Result<Option<Item>, LogError> {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Ok(Some(item));
            }
            if self.done {
                return Ok(None);
            }
            if self.cur < self.sources.len() {
                if let Some(item) = self.next_batch().await? {
                    self.sent_in_pass = true;
                    return Ok(Some(item));
                }
                self.cur += 1;
                self.index = None;
                continue;
            }
            if !self.live || self.last_pass {
                self.done = true;
                return Ok(None);
            }
            if !poll {
                return Ok(None);
            }
            if self.polled && !self.sent_in_pass {
                self.sent_in_pass = true;
                return Ok(Some(Item::Keepalive));
            }
            tokio::time::sleep(POLL).await;
            self.start_poll().await;
        }
    }

    /// How many times this stream has read a chunk index.
    pub fn index_reads(&self) -> u64 {
        self.index_reads
    }

    /// Check the tail lease of every source, mark the ones that are not
    /// running and still being stored, and say whether any is; queue a
    /// `storing` event when that changed. A failed check marks none.
    async fn check_storing(&mut self) -> bool {
        let resources: Vec<String> = self
            .sources
            .iter()
            .map(|src| tail_lease_resource(&self.run, &src.step, &src.attempt))
            .collect();
        let live = if resources.is_empty() {
            Vec::new()
        } else {
            self.db.live_leases(&resources).await.unwrap_or_default()
        };
        // Only a source that cannot grow otherwise counts: the running
        // attempt's tail always holds its lease while it reads.
        let mut any = false;
        for (src, resource) in self.sources.iter_mut().zip(&resources) {
            src.storing = !src.may_grow && live.contains(resource);
            any |= src.storing;
        }
        if any != self.storing_sent {
            self.storing_sent = any;
            self.pending.push_back(Item::Storing(any));
        }
        any
    }

    async fn start_poll(&mut self) {
        // A Run row that is gone ends the stream like a settled one.
        let terminal = match self.db.run_status(&self.run).await {
            Ok(Some(s)) => s.is_terminal(),
            Ok(None) => true,
            Err(_) => false,
        };
        // A failed refresh keeps the previous sources for this pass.
        let mut superseded = false;
        if let Ok((sources, latest)) = resolve(self.db.as_ref(), &self.run, &self.scope).await {
            self.sources = sources;
            if let Scope::Step {
                pinned: Some(p), ..
            } = &self.scope
            {
                superseded = latest.as_ref() != Some(p);
            }
        }
        let storing = self.check_storing().await;
        let ending = terminal || superseded;
        if !ending {
            // A rerun made the run go on: a later end waits STORE_WAIT anew.
            self.ending_since = None;
        }
        let waited_out = ending
            && storing
            && self.ending_since.get_or_insert_with(Instant::now).elapsed() >= STORE_WAIT;
        if waited_out {
            self.pending.push_back(Item::StoreWaitOver);
        }
        self.last_pass = ending && (!storing || waited_out);
        self.cur = 0;
        self.index = None;
        self.polled = true;
        self.sent_in_pass = false;
        self.pass += 1;
    }

    /// The next batch of the current source, or `None` when it has nothing
    /// (more) to send in this pass.
    async fn next_batch(&mut self) -> Result<Option<Item>, LogError> {
        let (step, attempt, may_grow) = {
            let src = &self.sources[self.cur];
            (src.step.clone(), src.attempt.clone(), src.grows())
        };
        let k = key(&step, &attempt);
        let from = self.progress.get(&k).copied().unwrap_or(0);
        if from == SKIPPED {
            return Ok(None);
        }
        // An attempt that is not running, sent in full, and quiet for SETTLE:
        // its index is not read again.
        if !may_grow
            && self
                .quiet_since
                .get(&k)
                .is_some_and(|t| t.elapsed() >= SETTLE)
        {
            return Ok(None);
        }
        if self.index.is_none() {
            // Only the rows not sent yet, plus the one before `from` (a
            // resumed stream may read it to join a `\r\n`).
            self.index = Some(
                self.logs
                    .chunks_from(&self.run, &step, &attempt, from.saturating_sub(1))
                    .await?,
            );
            self.index_reads += 1;
        }
        let index = self.index.as_deref().unwrap_or_default();
        let todo: Vec<&LogChunkMeta> = index.iter().filter(|c| c.seq >= from).collect();
        if todo.is_empty() {
            if may_grow {
                self.quiet_since.remove(&k);
            } else {
                self.quiet_since.entry(k).or_insert_with(Instant::now);
            }
            return Ok(None);
        }
        self.quiet_since.remove(&k);
        // A stream resumed from a cursor does not know how the chunk before
        // `from` ended; it reads that chunk if the batch starts with `\n`.
        let before = (from > 0 && !self.ends_cr.contains_key(&k))
            .then(|| index.iter().find(|c| c.seq + 1 == from).cloned())
            .flatten();
        let end = todo.last().map_or(from, |c| c.seq + 1);
        let strip = self.strip_lead.as_ref() == Some(&k);

        // The chunks this batch can take, fetched ahead in order. The loop
        // below reads at most the first chunk that takes its buffer past
        // BATCH_BYTES (and never past the second chunk); its buffer is at
        // most 3 bytes shorter than the chunks' lengths (a stripped cut
        // character), so it never reads past the first chunk whose end lies
        // past BATCH_BYTES + 3, or the second chunk when that is the first.
        let mut sum = 0u64;
        let take = todo
            .iter()
            .position(|c| {
                sum += c.len;
                sum > BATCH_BYTES + 3
            })
            .map_or(todo.len(), |k| (k.max(1) + 1).min(todo.len()));
        let mut bodies = self
            .logs
            .read_chunks(todo[..take].iter().map(|c| (*c).clone()).collect());

        let mut buf: Vec<u8> = Vec::new();
        let mut next = from;
        // (buf length, next seq) at the last chunk boundary on a whole character.
        let mut whole_at: Option<(usize, u64)> = None;
        // The batch went past BATCH_BYTES by one chunk to finish a character.
        let mut extended = false;
        // The batch stopped before the newest chunk.
        let mut cut_short = false;
        for (i, c) in todo.iter().enumerate() {
            if !buf.is_empty() && buf.len() as u64 + c.len > BATCH_BYTES {
                let ends_whole = whole_at.is_some_and(|(len, _)| len == buf.len());
                if ends_whole || extended {
                    cut_short = true;
                    break;
                }
                extended = true;
            }
            let Some(body) = bodies.next().await else {
                // Not reached (see `take`); stop as if capped.
                cut_short = true;
                break;
            };
            buf.extend(body?);
            next = c.seq + 1;
            if strip && i == 0 {
                let n = buf
                    .iter()
                    .take(3)
                    .take_while(|b| (0x80..0xC0).contains(*b))
                    .count();
                buf.drain(..n);
            }
            if incomplete_utf8_tail(&buf) == 0 {
                whole_at = Some((buf.len(), next));
            }
        }
        if incomplete_utf8_tail(&buf) == 0 {
            self.held.remove(&k);
        } else if cut_short {
            // Capped while still inside a character: end at the last whole
            // boundary, else send it with the cut bytes replaced.
            if let Some((len, n)) = whole_at {
                buf.truncate(len);
                next = n;
            }
        } else if self.live && !self.last_pass && may_grow {
            // The newest chunk of a running attempt ends inside a character:
            // send up to the last whole boundary and wait for the rest, for
            // at most HOLD_PASSES passes.
            let pass = self.pass;
            let held = self.held.entry(k.clone()).or_insert((end, pass));
            if held.0 != end {
                *held = (end, pass);
            }
            if pass - held.1 < u64::from(HOLD_PASSES) {
                let Some((len, n)) = whole_at else {
                    return Ok(None);
                };
                buf.truncate(len);
                next = n;
            } else {
                self.held.remove(&k);
            }
        }
        if strip {
            self.strip_lead = None;
        }
        // An empty batch (empty chunks) leaves the last byte sent as it was.
        let sends_bytes = !buf.is_empty();
        if buf.first() == Some(&b'\n') {
            let after_cr = match (self.ends_cr.get(&k), before) {
                (Some(&cr), _) => cr,
                (None, Some(meta)) => self.logs.read_chunk(&meta).await?.last() == Some(&b'\r'),
                (None, None) => false,
            };
            if after_cr {
                buf.remove(0);
            }
        }
        if sends_bytes {
            self.ends_cr.insert(k.clone(), buf.last() == Some(&b'\r'));
        }
        crlf_to_lf(&mut buf);
        self.progress.insert(k, next);
        Ok(Some(Item::Data {
            id: Cursor::format(&attempt, next),
            text: String::from_utf8_lossy(&buf).into_owned(),
        }))
    }
}

/// The ordered sources for a scope, and the step's latest attempt (Step scope).
async fn resolve(
    db: &dyn Db,
    run: &RunId,
    scope: &Scope,
) -> Result<(Vec<Source>, Option<AttemptId>), LogError> {
    match scope {
        Scope::Fixed { step, attempt } => Ok((
            vec![Source {
                step: step.clone(),
                attempt: attempt.clone(),
                may_grow: true,
                storing: false,
            }],
            None,
        )),
        Scope::Run => {
            let mut out = Vec::new();
            for s in db.steps_of_run(run).await? {
                let last = s.attempts.len().saturating_sub(1);
                for (i, a) in s.attempts.iter().enumerate() {
                    out.push(Source {
                        step: s.step.clone(),
                        attempt: a.id.clone(),
                        may_grow: i == last && a.outcome == AttemptOutcome::Running,
                        storing: false,
                    });
                }
            }
            Ok((out, None))
        }
        Scope::Step {
            step,
            read_step,
            pinned,
        } => {
            let attempts = db
                .steps_of_run(run)
                .await?
                .into_iter()
                .find(|s| &s.step == step)
                .map(|s| s.attempts)
                .unwrap_or_default();
            let latest = attempts.last().map(|a| a.id.clone());
            let out = attempts
                .into_iter()
                .filter(|a| pinned.as_ref().is_none_or(|p| p == &a.id))
                .map(|a| Source {
                    may_grow: Some(&a.id) == latest.as_ref()
                        && a.outcome == AttemptOutcome::Running,
                    storing: false,
                    step: read_step.clone(),
                    attempt: a.id,
                })
                .collect();
            Ok((out, latest))
        }
    }
}

/// Replace each `\r\n` in `buf` with `\n`, in place. A lone `\r` stays.
pub fn crlf_to_lf(buf: &mut Vec<u8>) {
    let mut w = 0;
    for r in 0..buf.len() {
        if buf[r] == b'\r' && buf.get(r + 1) == Some(&b'\n') {
            continue;
        }
        buf[w] = buf[r];
        w += 1;
    }
    buf.truncate(w);
}

/// How many trailing bytes of `buf` are the start of a UTF-8 character whose
/// remaining bytes are missing (0 when it ends on a whole character, or on
/// bytes that are not UTF-8 at all).
pub fn incomplete_utf8_tail(buf: &[u8]) -> usize {
    let floor = buf.len().saturating_sub(4);
    let Some(lead) = (floor..buf.len())
        .rev()
        .find(|&i| !(0x80..0xC0).contains(&buf[i]))
    else {
        return 0;
    };
    match std::str::from_utf8(&buf[lead..]) {
        Err(e) if e.valid_up_to() == 0 && e.error_len().is_none() => buf.len() - lead,
        _ => 0,
    }
}

/// Open a stream and serve it as SSE. The first batch is read before the
/// response starts, so a log whose first promised chunk will not read is a 500
/// (ADR-0063 part 5); a later failure is an `event: error` and the end.
pub async fn sse(
    st: &crate::AppState,
    run: RunId,
    scope: Scope,
    start: Start,
    live: bool,
) -> Result<Sse<BoxStream<'static, Result<Event, Infallible>>>, ApiError> {
    let mut s = LogStream::open(st.logs.clone(), st.db.clone(), run, scope, start, live).await?;
    let mut head = Vec::new();
    while let Some(item) = s.next(false).await? {
        let data = matches!(item, Item::Data { .. });
        head.push(item);
        if data {
            break;
        }
    }
    let rest = stream::unfold(s, |mut s| async move {
        match s.next(true).await {
            Ok(Some(item)) => Some((item, s)),
            Ok(None) => None,
            Err(e) => {
                s.done = true;
                s.pending.clear();
                Some((Item::Error(format!("log storage read failed: {e}")), s))
            }
        }
    });
    let events = stream::iter(head)
        .chain(rest)
        .map(|item| Ok(item.into_event()));
    Ok(Sse::new(events.boxed()))
}

/// Read the `?from=` cursor, else the `Last-Event-ID` header. `Ok(None)` when
/// neither is present; a malformed one is a 400.
pub fn cursor_from(
    from: Option<&str>,
    headers: &axum::http::HeaderMap,
) -> Result<Option<Cursor>, ApiError> {
    let raw = match from {
        Some(f) => Some(f.to_string()),
        None => headers
            .get("last-event-id")
            .map(|v| v.to_str().map(str::to_string))
            .transpose()
            .map_err(|_| ApiError::BadRequest("Last-Event-ID is not text".into()))?,
    };
    match raw {
        None => Ok(None),
        Some(r) => Cursor::parse(&r)
            .map(Some)
            .ok_or_else(|| ApiError::BadRequest(format!("malformed log cursor {r:?}"))),
    }
}

/// The replay start for a route that takes a cursor and a tail: the cursor
/// wins; `tail` applies only without one.
pub fn start_of(cursor: Option<Cursor>, tail: Option<u64>) -> Start {
    match (cursor, tail) {
        (Some(c), _) => Start::From(c),
        (None, Some(t)) => Start::Tail(t),
        (None, None) => Start::Full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let c = Cursor::parse("a2:7").unwrap();
        assert_eq!(c.attempt, AttemptId("a2".into()));
        assert_eq!(c.next_seq, 7);
        assert_eq!(Cursor::format(&c.attempt, c.next_seq), "a2:7");
        // An attempt id holding a colon keeps it; the seq is after the last.
        assert_eq!(
            Cursor::parse("t:1:3").unwrap().attempt,
            AttemptId("t:1".into())
        );
        for bad in ["", "a2", ":3", "a2:", "a2:x", "a2:-1"] {
            assert!(Cursor::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn crlf_becomes_lf_and_a_lone_cr_stays() {
        let norm = |s: &[u8]| {
            let mut v = s.to_vec();
            crlf_to_lf(&mut v);
            v
        };
        assert_eq!(norm(b"a\r\nb\r\n"), b"a\nb\n");
        assert_eq!(norm(b"50%\r60%\r\n"), b"50%\r60%\n");
        assert_eq!(norm(b"\r\r\n\n"), b"\r\n\n");
        assert_eq!(norm(b"end\r"), b"end\r");
        assert_eq!(norm(b""), b"");
    }

    #[test]
    fn incomplete_tail_counts_only_a_cut_character() {
        let euro = "€".as_bytes(); // 3 bytes
        assert_eq!(incomplete_utf8_tail(b"abc"), 0);
        assert_eq!(incomplete_utf8_tail(euro), 0);
        assert_eq!(incomplete_utf8_tail(&euro[..1]), 1);
        assert_eq!(incomplete_utf8_tail(&[b'x', euro[0], euro[1]]), 2);
        // Invalid bytes are not "waiting for more".
        assert_eq!(incomplete_utf8_tail(&[0xFF]), 0);
        assert_eq!(incomplete_utf8_tail(&[b'x', 0x80]), 0);
        assert_eq!(incomplete_utf8_tail(b""), 0);
    }
}
