//! `kill-tailer` (git-bug dbb1d7c): ADR-0077 rank 2 for log storage. At two
//! server replicas, SIGKILL the replica that holds a step's tail lease
//! (`tail2:<run>:<step>:<attempt>`: it reads the step's log and appends it to
//! the store) while the step prints, then check what was stored.
//!
//! **The workload.** A step can be given a secret only in an Environment, and
//! an Environment needs a Project, so the pipeline is a repo pipeline, as the
//! Cache probe's is (`crate::cache`): [`LogProbe::pipeline_yaml`] is written
//! beside the probe's in the in-cluster Forgejo repo, the harness creates
//! Environment [`ENVIRONMENT`] (no approvers, every Project admitted) and
//! stores the secret [`SECRET_NAME`] there, and each Run is dispatched through
//! the API. Step `produce` prints [`LINES`] numbered lines, one a second
//! (`L <run> <000001..>`); every [`SECRET_EVERY`]-th line ends in ` S
//! <secret>`. It prints for longer than a takeover of the tail lease takes
//! (its TTL is 45 s). It then writes a file `consume` checks, so a Run has the
//! two Steps the other invariants expect. The secret is random, drawn from
//! the seed ([`secret_for_seed`]), and in the scenario file.
//!
//! **The invariants** ([`evaluate`], pure over the gathered [`LogFacts`]):
//!
//! | id  | invariant |
//! |-----|-----------|
//! | LG1 | per counting stream (the `produce` Attempt's log), the current part (the output rows after its newest `source_restarted` row) is lines 1 to [`LINES`], each whole, in order, none twice, the secret ones ending in ` S ***`; or the stream ends in an `incomplete` or `not_stored` row |
//! | LG2 | per stream, `seq` is 0..n with no gap, each row's `byte_offset` is the previous row's plus its `len`, and each body is `len` bytes with the row's digest |
//! | LG3 | the secret is in no object under `logs/<run>/` (objects no row points at included: an append killed or fenced out after its PUT leaves one) and not in `GET /v1/runs/{id}/logs`; a whole counting part carries the placeholder `***` once per secret line |
//!
//! The scenario is INCONCLUSIVE when the kill did not exercise a takeover:
//! the target stream's output rows carry one `writer_term` only (nothing was
//! stored by a writer after the kill), no counting stream was stored whole
//! (markers only), the placeholder is in no whole part (the secret was never
//! printed, so LG3 could find nothing), or LG1 or LG3 looked at fewer streams
//! or Runs than the workload has.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read as _;
use std::time::Duration;

use futures::StreamExt as _;
use object_store::{path::Path as ObjPath, ObjectStore as _, ObjectStoreExt as _};
use rand::{Rng as _, SeedableRng as _};
use serde::{Deserialize, Serialize};

use crate::check::{Facts, Finding, Severity};
use crate::env::Api;
use crate::pods::{PodView, Views};
use crate::scenario::Trigger;
use crate::time::now_ms;

/// The step that prints the counted lines.
pub const LOG_STEP: &str = "produce";
/// How many lines it prints, one a second.
pub const LINES: u32 = 150;
/// Every this-many-th line carries the secret.
pub const SECRET_EVERY: u32 = 10;
/// The pipeline (`.scarab/<name>.yaml` in `crate::cache::REPO`).
pub const PIPELINE: &str = "chaos-logs";
/// The org Environment the step runs in.
pub const ENVIRONMENT: &str = "chaos-logs";
/// The secret's name in that Environment.
pub const SECRET_NAME: &str = "CHAOS_LOG_SECRET";
/// What the server stores in place of a secret value.
pub const PLACEHOLDER: &str = "***";
/// The fewest output rows of the target stream before the kill: the tail is
/// appending, not about to start.
pub const TRIGGER_ROWS: i64 = 2;
const DISPATCH_REASON: &str = "scarab-chaos kill-tailer";

/// `kill-tailer`'s workload as planned. Stored in the scenario file, so a
/// replay prints the same lines with the same secret.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LogProbe {
    pub lines: u32,
    pub every: u32,
    /// The planted value. A test value, made for this scenario only.
    pub secret: String,
    pub pipeline_yaml: String,
    /// The Project the Runs were dispatched in (`org/project`).
    #[serde(default)]
    pub project: Option<String>,
}

impl LogProbe {
    pub fn new(seed: u64, image: &str) -> LogProbe {
        LogProbe {
            lines: LINES,
            every: SECRET_EVERY,
            secret: secret_for_seed(seed),
            pipeline_yaml: pipeline_yaml(image, LINES, SECRET_EVERY),
            project: None,
        }
    }

    /// The same lines, secret and pipeline, nothing observed (a replay).
    pub fn replay(&self) -> LogProbe {
        LogProbe {
            project: None,
            ..self.clone()
        }
    }
}

/// The secret of seed `seed`: `chaoslog` and 32 hex digits from its own RNG
/// stream, so the scenario's plan draws do not move. 128 random bits do not
/// occur in a log by chance, and hex passes through JSON and SSE unchanged.
pub fn secret_for_seed(seed: u64) -> String {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed ^ 0x6b69_6c6c_7461_696c);
    let bytes: [u8; 16] = rng.gen();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("chaoslog{hex}")
}

/// The pipeline: `produce` prints the counted lines and writes their count to
/// the workspace; `consume` checks it.
pub fn pipeline_yaml(image: &str, lines: u32, every: u32) -> String {
    format!(
        r#"# Written by scarab-chaos (crates/scarab-chaos/src/logs.rs): the workload
# of `just test chaos kill-tailer`. Dispatched through the API; no push trigger.
on:
  api: {{}}
environment: {ENVIRONMENT}
steps:
  - id: {LOG_STEP}
    image: {image}
    secrets: [{SECRET_NAME}]
    command:
      - sh
      - -c
      - |
        s="$(cat "$SCARAB_SECRETS/{SECRET_NAME}")" || exit 3
        [ -n "$s" ] || exit 4
        i=1
        while [ "$i" -le {lines} ]; do
          if [ $((i % {every})) -eq 0 ]; then
            printf 'L %s %06d S %s\n' "$SCARAB_RUN" "$i" "$s"
          else
            printf 'L %s %06d\n' "$SCARAB_RUN" "$i"
          fi
          i=$((i + 1))
          sleep 1
        done
        printf '%s\n' {lines} > /workspace/COUNTED
  - id: consume
    image: {image}
    needs: [{LOG_STEP}]
    command:
      - sh
      - -c
      - |
        test "$(cat /workspace/COUNTED)" = {lines} && echo CONSUMED-OK
"#
    )
}

// --- the live half -------------------------------------------------------------------

/// Before the Cache probe's seed: write the pipeline beside the probe's.
pub async fn write_pipeline(fj: &crate::cache::Forgejo, p: &LogProbe) -> Result<(), String> {
    crate::cache::write_repo_pipeline(
        fj,
        PIPELINE,
        &p.pipeline_yaml,
        "scarab-chaos: the kill-tailer workload",
    )
    .await
}

/// The Project the Runs are dispatched in, its Environment and the secret.
/// `cache_project` is the Project the Cache probe bound (`org/project`); when
/// the probe did not bind one, the repo is bound here, through a fresh
/// connection (as the probe does). Returns (org, project).
pub async fn prepare(
    api: &Api,
    fj: &crate::cache::Forgejo,
    p: &mut LogProbe,
    cache_project: Option<&str>,
) -> Result<(String, String), String> {
    let (org, project) = match cache_project.and_then(|x| x.split_once('/')) {
        Some((org, project)) => (org.to_string(), project.to_string()),
        None => {
            let conn = match api.connection_to(&fj.server_url).await? {
                Some(c) => c,
                None => {
                    api.recreate_forgejo_connection(&fj.owner, &fj.server_url, &fj.token)
                        .await?
                }
            };
            api.bind_repo(&conn, &fj.owner, crate::cache::REPO).await?
        }
    };
    api.put_open_environment(
        &org,
        ENVIRONMENT,
        "scarab-chaos kill-tailer: the step's secret",
    )
    .await?;
    api.put_environment_secret(&org, &project, ENVIRONMENT, SECRET_NAME, &p.secret)
        .await?;
    p.project = Some(format!("{org}/{project}"));
    Ok((org, project))
}

/// Dispatch one Run of the pipeline. Returns its id.
pub async fn dispatch(api: &Api, org: &str, project: &str) -> Result<String, String> {
    api.dispatch(
        org,
        project,
        PIPELINE,
        "main",
        &serde_json::json!({}),
        DISPATCH_REASON,
    )
    .await
}

/// The tail lease of a step Attempt's main stream
/// (`scarab_engine::tail_lease_resource`, which this crate does not link).
pub fn tail_resource(run: &str, step: &str, attempt: &str) -> String {
    format!("tail2:{run}:{step}:{attempt}")
}

/// The lease row `resource`: (owner, term).
pub async fn tail_lease(
    pg: &sqlx::PgPool,
    resource: &str,
) -> Result<Option<(String, Option<i64>)>, String> {
    sqlx::query_as::<_, (String, Option<i64>)>(
        "SELECT owner, term::bigint FROM leases WHERE resource = $1",
    )
    .bind(resource)
    .fetch_optional(pg)
    .await
    .map_err(|e| format!("postgres: {e}"))
}

/// `output` rows stored for one stream.
pub async fn output_rows(
    pg: &sqlx::PgPool,
    run: &str,
    step: &str,
    attempt: &str,
) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM log_chunks
         WHERE run_id = $1 AND step_id = $2 AND attempt_id = $3 AND kind = 'output'",
    )
    .bind(run)
    .bind(step)
    .bind(attempt)
    .fetch_one(pg)
    .await
    .map_err(|e| format!("postgres: {e}"))
}

/// Streams of `runs` still `open`.
pub async fn open_streams(pg: &sqlx::PgPool, runs: &[String]) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM log_streams WHERE run_id = ANY($1) AND state = 'open'",
    )
    .bind(runs)
    .fetch_one(pg)
    .await
    .map_err(|e| format!("postgres: {e}"))
}

/// After the step-running trigger: wait until the target stream has
/// [`TRIGGER_ROWS`] output rows while the step still runs. Bound: the step's
/// print time. `None` when the step stopped running or the bound ran out
/// first.
pub async fn wait_output_rows(
    pg: &sqlx::PgPool,
    views: &Views,
    t: Trigger,
    holds: &impl Fn(&PodView) -> bool,
    lines: u32,
) -> Result<Option<Trigger>, String> {
    let attempt = t
        .attempt
        .clone()
        .ok_or("the target Pod has no attempt label")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(u64::from(lines));
    loop {
        let rows = output_rows(pg, &t.run, &t.step, &attempt).await?;
        if rows >= TRIGGER_ROWS {
            return Ok(Some(Trigger {
                condition: format!("step container running, {rows} output rows of its log stored"),
                observed_at_ms: now_ms(),
                ..t
            }));
        }
        let held = views
            .lock()
            .expect("views lock")
            .get(&t.pod_uid)
            .is_some_and(holds);
        if !held || std::time::Instant::now() > deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// --- facts ---------------------------------------------------------------------------

/// One `log_chunks` row and, for an output row, its body as read back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogRow {
    pub seq: i64,
    pub byte_offset: i64,
    pub len: i64,
    /// `output`, `source_restarted`, `not_stored` or `incomplete`.
    pub kind: String,
    pub reason: Option<String>,
    pub writer_term: Option<i64>,
    pub object_key: Option<String>,
    /// The row's SHA-256, hex.
    pub digest: Option<String>,
    /// The body, decompressed. `None` for a marker.
    pub body: Option<Result<Vec<u8>, String>>,
}

/// One stored log and its rows, in `seq` order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogStream {
    pub run: String,
    pub step: String,
    pub attempt: String,
    /// `log_streams.state`; `None` when the stream has rows but no row there.
    pub state: Option<String>,
    pub rows: Vec<LogRow>,
}

/// One object under a Run's log prefix, decompressed.
#[derive(Debug, Clone, PartialEq)]
pub struct LogObject {
    pub run: String,
    pub key: String,
    pub bytes: Result<Vec<u8>, String>,
}

/// What the checker read for `kill-tailer`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogFacts {
    pub secret: String,
    pub lines: u32,
    pub every: u32,
    /// The step whose streams LG1 checks ([`LOG_STEP`]).
    pub step: String,
    pub streams: Vec<LogStream>,
    pub objects: Vec<LogObject>,
    /// Per scenario Run: whether its prefix `logs/<run>/` was listed.
    pub listed: Vec<(String, Result<(), String>)>,
    /// Per scenario Run: `GET /v1/runs/{id}/logs`.
    pub api: Vec<(String, Result<String, String>)>,
}

/// SHA-256 of `data`, hex (the form [`LogRow::digest`] holds).
pub fn digest_hex(data: &[u8]) -> String {
    scarab_storage::sha256_hex(data)
}

/// A stored body as the server wrote it: gzip (`logs.rs` in scarab-server).
/// Anything that is not gzip is returned as it is.
fn decompress(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_end(&mut out)
            .map_err(|e| format!("gunzip: {e}"))?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

async fn read_object(bucket: &object_store::aws::AmazonS3, key: &str) -> Result<Vec<u8>, String> {
    let got = bucket
        .get(&ObjPath::from(key))
        .await
        .map_err(|e| format!("GET {key}: {e}"))?;
    let bytes = got.bytes().await.map_err(|e| format!("GET {key}: {e}"))?;
    decompress(&bytes)
}

/// Read every stream of `runs` with its rows and bodies, every object under
/// each Run's log prefix, and each Run's log through the API.
pub async fn gather(
    pg: &sqlx::PgPool,
    bucket: &object_store::aws::AmazonS3,
    api: &Api,
    runs: &[String],
    p: &LogProbe,
) -> Result<LogFacts, String> {
    let db = |e: sqlx::Error| format!("postgres: {e}");
    let mut f = LogFacts {
        secret: p.secret.clone(),
        lines: p.lines,
        every: p.every,
        step: LOG_STEP.into(),
        ..Default::default()
    };

    // Every object under each Run's prefix, read once.
    let mut read: HashMap<String, Result<Vec<u8>, String>> = HashMap::new();
    for run in runs {
        let prefix = ObjPath::from(format!("logs/{run}"));
        let mut keys = Vec::new();
        let mut listing = bucket.list(Some(&prefix));
        let mut listed = Ok(());
        while let Some(m) = listing.next().await {
            match m {
                Ok(m) => keys.push(m.location.to_string()),
                Err(e) => {
                    listed = Err(format!("list {prefix}: {e}"));
                    break;
                }
            }
        }
        f.listed.push((run.clone(), listed));
        for key in keys {
            let bytes = read_object(bucket, &key).await;
            read.insert(key.clone(), bytes.clone());
            f.objects.push(LogObject {
                run: run.clone(),
                key,
                bytes,
            });
        }
        f.api.push((run.clone(), api.logs_text(run).await));
    }

    let states: HashMap<(String, String, String), String> =
        sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT run_id, step_id, attempt_id, state FROM log_streams WHERE run_id = ANY($1)",
        )
        .bind(runs)
        .fetch_all(pg)
        .await
        .map_err(db)?
        .into_iter()
        .map(|(r, s, a, state)| ((r, s, a), state))
        .collect();
    type Row = (
        String,
        String,
        String,
        i64,
        i64,
        i64,
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<Vec<u8>>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT run_id, step_id, attempt_id, seq, byte_offset, len, kind, reason, writer_term,
                object_key, digest
         FROM log_chunks WHERE run_id = ANY($1) ORDER BY run_id, step_id, attempt_id, seq",
    )
    .bind(runs)
    .fetch_all(pg)
    .await
    .map_err(db)?;
    let mut streams: BTreeMap<(String, String, String), Vec<LogRow>> = BTreeMap::new();
    for (run, step, attempt, seq, byte_offset, len, kind, reason, writer_term, key, digest) in rows
    {
        let body = match &key {
            Some(k) if kind == "output" => Some(match read.get(k) {
                Some(b) => b.clone(),
                None => read_object(bucket, k).await,
            }),
            _ => None,
        };
        streams
            .entry((run, step, attempt))
            .or_default()
            .push(LogRow {
                seq,
                byte_offset,
                len,
                kind,
                reason,
                writer_term,
                object_key: key,
                digest: digest.map(|d| d.iter().map(|b| format!("{b:02x}")).collect()),
                body,
            });
    }
    for key in states.keys() {
        streams.entry(key.clone()).or_default();
    }
    for ((run, step, attempt), rows) in streams {
        let state = states
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .cloned();
        f.streams.push(LogStream {
            run,
            step,
            attempt,
            state,
            rows,
        });
    }
    Ok(f)
}

// --- evaluate (pure) -------------------------------------------------------------------

fn finding(inv: &str, severity: Severity, detail: String) -> Finding {
    Finding {
        invariant: inv.into(),
        severity,
        detail,
    }
}

/// `text` with the secret replaced, so a finding never repeats it.
fn masked(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "<secret>")
    }
}

/// The current part's lines against the counted lines of `run`: Ok(the
/// placeholders seen), or what is wrong (the first problem).
pub fn check_lines(run: &str, text: &str, lines: u32, every: u32) -> Result<usize, String> {
    if text.is_empty() {
        return Err("no line is stored".into());
    }
    if !text.ends_with('\n') {
        let last = text.rsplit('\n').next().unwrap_or_default();
        return Err(format!(
            "the stored text ends inside a line ({last:?}) with no marker after it"
        ));
    }
    let mut n: u32 = 0;
    let mut placeholders = 0;
    for line in text.split_terminator('\n') {
        n += 1;
        if n > lines {
            return Err(format!(
                "line {n} ({line:?}) is past the last line, {lines}"
            ));
        }
        let want = format!("L {run} {n:06}");
        let rest = line.strip_prefix(want.as_str()).ok_or_else(|| {
            format!("line {n} is {line:?}, not {want:?}: a line is torn, missing, out of order or repeated")
        })?;
        match rest {
            "" => {}
            r if every > 0 && n.is_multiple_of(every) && r == format!(" S {PLACEHOLDER}") => {
                placeholders += 1
            }
            _ => {
                return Err(format!(
                    "line {n} is {line:?}: after {want:?} only {:?} may follow, on every {every}th line",
                    format!(" S {PLACEHOLDER}")
                ))
            }
        }
    }
    if n < lines {
        return Err(format!(
            "lines 1 to {n} of {lines} are stored and the stream ends in no marker"
        ));
    }
    Ok(placeholders)
}

/// LG1, LG2 and LG3 over `l`, with the Attempts and the faults from `f`.
/// Returns the findings, what each invariant looked at, and why the scenario
/// does not count as a test (empty when it does).
pub fn evaluate(f: &Facts, l: &LogFacts) -> (Vec<Finding>, BTreeMap<String, usize>, Vec<String>) {
    let mut findings = Vec::new();
    let mut checked = BTreeMap::new();
    let mut why = Vec::new();
    let secret = l.secret.as_str();
    let stream_of = |run: &str, step: &str, att: &str| {
        l.streams
            .iter()
            .find(|s| s.run == run && s.step == step && s.attempt == att)
    };

    // LG2: every stream's rows are contiguous and their bodies match them.
    for s in &l.streams {
        let mut rows: Vec<&LogRow> = s.rows.iter().collect();
        rows.sort_by_key(|r| r.seq);
        let name = format!("run {} step {} attempt {}", s.run, s.step, s.attempt);
        let mut offset = 0i64;
        for (i, r) in rows.iter().enumerate() {
            if r.seq != i as i64 {
                findings.push(finding(
                    "LG2",
                    Severity::Fail,
                    format!("{name}: row {i} has seq {}, a gap in the stream", r.seq),
                ));
                break;
            }
            if r.byte_offset != offset {
                findings.push(finding(
                    "LG2",
                    Severity::Fail,
                    format!(
                        "{name}: seq {} starts at byte {}, the rows before it end at {offset}",
                        r.seq, r.byte_offset
                    ),
                ));
                break;
            }
            offset = r.byte_offset + r.len;
            if let Some(Ok(body)) = &r.body {
                if body.len() as i64 != r.len {
                    findings.push(finding(
                        "LG2",
                        Severity::Fail,
                        format!(
                            "{name}: seq {}'s body is {} bytes, its row says {}",
                            r.seq,
                            body.len(),
                            r.len
                        ),
                    ));
                }
                if let Some(d) = &r.digest {
                    if *d != digest_hex(body) {
                        findings.push(finding(
                            "LG2",
                            Severity::Fail,
                            format!("{name}: seq {}'s body does not hash to its digest", r.seq),
                        ));
                    }
                }
            }
        }
    }
    checked.insert("LG2".into(), l.streams.len());

    // LG1: each counting stream is whole in its current part, or marked.
    let mut lg1 = 0;
    let mut whole = 0;
    let mut placeholders = 0;
    let want_placeholders = l.lines.checked_div(l.every).unwrap_or(0) as usize;
    for a in f.attempts.iter().filter(|a| a.step == l.step) {
        lg1 += 1;
        let name = format!("run {} step {} attempt {}", a.run, a.step, a.attempt);
        let Some(s) = stream_of(&a.run, &a.step, &a.attempt) else {
            findings.push(finding(
                "LG1",
                Severity::Fail,
                format!("{name}: no log was stored for it and no marker says why"),
            ));
            continue;
        };
        let mut rows: Vec<&LogRow> = s.rows.iter().collect();
        rows.sort_by_key(|r| r.seq);
        if let Some(last) = rows
            .last()
            .filter(|r| r.kind == "incomplete" || r.kind == "not_stored")
        {
            findings.push(finding(
                "LG1",
                Severity::Info,
                format!(
                    "{name}: the stream ends in a `{}` row{} (allowed: marked, not whole)",
                    last.kind,
                    last.reason
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                ),
            ));
            continue;
        }
        let restarted = rows
            .iter()
            .filter(|r| r.kind == "source_restarted")
            .map(|r| r.seq)
            .max();
        let mut text = Vec::new();
        let mut unreadable = None;
        for r in rows
            .iter()
            .filter(|r| r.kind == "output" && restarted.is_none_or(|x| r.seq > x))
        {
            match &r.body {
                Some(Ok(b)) => text.extend_from_slice(b),
                Some(Err(e)) => {
                    unreadable = Some(format!("seq {}'s body cannot be read: {e}", r.seq));
                    break;
                }
                None => {
                    unreadable = Some(format!("seq {} is an output row with no body", r.seq));
                    break;
                }
            }
        }
        if let Some(e) = unreadable {
            findings.push(finding("LG1", Severity::Fail, format!("{name}: {e}")));
            continue;
        }
        let text = String::from_utf8_lossy(&text);
        match check_lines(&a.run, &text, l.lines, l.every) {
            Err(e) => findings.push(finding(
                "LG1",
                Severity::Fail,
                masked(&format!("{name}: {e}"), secret),
            )),
            Ok(n) => {
                whole += 1;
                placeholders += n;
                if n != 0 && n != want_placeholders {
                    findings.push(finding(
                        "LG3",
                        Severity::Fail,
                        format!(
                            "{name}: the placeholder is on {n} lines of the whole log, not on all \
                             {want_placeholders} secret lines"
                        ),
                    ));
                }
            }
        }
        if s.state.as_deref() == Some("open") {
            findings.push(finding(
                "LG1",
                Severity::Warn,
                format!("{name}: the stream is still `open` after quiesce"),
            ));
        }
    }
    checked.insert("LG1".into(), lg1);

    // LG3: the secret in no stored object and not in the API's log.
    let referenced: BTreeSet<&str> = l
        .streams
        .iter()
        .flat_map(|s| s.rows.iter().filter_map(|r| r.object_key.as_deref()))
        .collect();
    for o in &l.objects {
        match &o.bytes {
            Ok(b) if !secret.is_empty() && contains(b, secret.as_bytes()) => {
                findings.push(finding(
                    "LG3",
                    Severity::Fail,
                    format!(
                        "run {}: object {} ({}) holds the secret",
                        o.run,
                        o.key,
                        if referenced.contains(o.key.as_str()) {
                            "a row points at it"
                        } else {
                            "no row points at it"
                        }
                    ),
                ))
            }
            Ok(_) => {}
            Err(e) => why.push(format!(
                "LG3: object {} of run {} could not be read: {e}",
                o.key, o.run
            )),
        }
    }
    let mut lg3 = 0;
    for (run, listed) in &l.listed {
        let api = l.api.iter().find(|(r, _)| r == run).map(|(_, t)| t);
        match (listed, api) {
            (Ok(()), Some(Ok(text))) => {
                lg3 += 1;
                if !secret.is_empty() && text.contains(secret) {
                    findings.push(finding(
                        "LG3",
                        Severity::Fail,
                        format!("run {run}: GET /v1/runs/{run}/logs holds the secret"),
                    ));
                }
            }
            (Err(e), _) => why.push(format!(
                "LG3: the log prefix of run {run} was not listed: {e}"
            )),
            (_, Some(Err(e))) => {
                why.push(format!("LG3: the API log of run {run} was not read: {e}"))
            }
            (_, None) => why.push(format!("LG3: the API log of run {run} was not read")),
        }
    }
    checked.insert("LG3".into(), lg3);

    // Exercised: a writer after the kill stored rows of the target stream.
    for t in f.faults.iter().filter_map(|x| x.trigger.as_ref()) {
        let Some(att) = &t.attempt else { continue };
        let terms: BTreeSet<i64> = stream_of(&t.run, &t.step, att)
            .map(|s| {
                s.rows
                    .iter()
                    .filter(|r| r.kind == "output")
                    .filter_map(|r| r.writer_term)
                    .collect()
            })
            .unwrap_or_default();
        if terms.len() < 2 {
            why.push(format!(
                "run {} step {} attempt {}: its output rows carry {} writer term(s) {terms:?}: no row \
                 was stored by a writer after the kill, so no takeover was tested",
                t.run,
                t.step,
                att,
                terms.len()
            ));
        }
    }
    if whole == 0 {
        why.push(
            "no counting stream was stored whole (each ends in a marker, or none was read): LG1 and \
             LG3's placeholder count checked no stored line"
                .into(),
        );
    } else if placeholders == 0 {
        why.push(format!(
            "the placeholder {PLACEHOLDER:?} is on no line of the {whole} whole log(s): the secret was \
             never printed, so LG3 could not have found it"
        ));
    }
    let runs = f.expected_runs;
    for (inv, n, what) in [
        ("LG1", lg1, "the counting stream of every Run"),
        ("LG3", lg3, "every Run's log prefix and API log"),
    ] {
        if n < runs {
            why.push(format!(
                "{inv} looked at {n}, below its minimum of {runs} ({what})"
            ));
        }
    }
    (findings, checked, why)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_gives_one_secret_and_seeds_differ() {
        assert_eq!(secret_for_seed(7), secret_for_seed(7));
        assert_ne!(secret_for_seed(7), secret_for_seed(8));
        assert_eq!(secret_for_seed(1).len(), "chaoslog".len() + 32);
    }

    /// The pipeline compiles: an API trigger, the Environment, the secret on
    /// the counting step, and `consume` after it.
    #[test]
    fn the_log_pipeline_compiles_in_its_environment_with_the_secret() {
        let ir =
            scarab_pipeline::compile_yaml(&pipeline_yaml("scarab-clone:dev", LINES, SECRET_EVERY))
                .unwrap_or_else(|e| panic!("{e:?}"));
        assert!(ir.triggers.0.contains_key("api"));
        assert_eq!(ir.environment.as_deref(), Some(ENVIRONMENT));
        let produce = ir.steps.iter().find(|s| s.id == LOG_STEP).expect("produce");
        assert_eq!(produce.secrets.len(), 1);
        assert_eq!(produce.secrets[0].name, SECRET_NAME);
        let consume = ir
            .steps
            .iter()
            .find(|s| s.id == "consume")
            .expect("consume");
        assert_eq!(consume.needs.0, vec![LOG_STEP.to_string()]);
    }

    #[test]
    fn counted_lines() {
        let text: String = (1..=4)
            .map(|n| {
                if n % 2 == 0 {
                    format!("L r {n:06} S ***\n")
                } else {
                    format!("L r {n:06}\n")
                }
            })
            .collect();
        assert_eq!(check_lines("r", &text, 4, 2), Ok(2));
        // The secret suffix is optional on a secret line, never elsewhere.
        assert_eq!(check_lines("r", &text.replace(" S ***", ""), 4, 2), Ok(0));
        assert!(check_lines("r", "L r 000001 S ***\n", 1, 2).is_err());
        assert!(check_lines("r", &text[..text.len() - 1], 4, 2)
            .unwrap_err()
            .contains("ends inside a line"));
        assert!(check_lines("r", "L r 000001\n", 4, 2)
            .unwrap_err()
            .contains("ends in no marker"));
        assert!(
            check_lines("x", &text, 4, 2).is_err(),
            "another Run's lines"
        );
    }
}
