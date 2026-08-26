//! `scarab-wsfetch` — the ADR-0061 workspace helper: `fetch` (default),
//! `hold`, and `drain`.
//!
//! Three modes, one binary, because they are the three lives of the same
//! credential and the same client:
//!
//! - **`fetch`** (and a bare invocation — see [`main`]): the s3-feed init
//!   container. Reads the Step's input **Workspace Snapshot** roots from its
//!   own environment, presents the tmpfs workspace token, and materialises
//!   each root — **in order** — into `/workspace`. Then it exits. No marker
//!   file, no wait loop, no `kubectl exec` anywhere in the path.
//! - **`hold`**: the egress doorstop (stage-1 drain). Ignores SIGTERM and
//!   waits for the control plane's egress-done marker — replacing the busybox
//!   `sh` loop, which died on TERM and silently destroyed the drain window.
//! - **`drain`**: the in-Pod drain. Ingests `/workspace` to the Depot's warm
//!   tier, prunes to declared `outputs:` in-process, and posts a
//!   [`DrainRecord`](scarab_workspace_client::DrainRecord) as the LAST act —
//!   the Depot is the rendezvous, and the control plane exchanges root hashes
//!   only, never bytes over `exec`.
//!
//! # What this replaces, and why it is not a performance story
//!
//! Before this binary, `drive_workspace` materialised every input into a tempdir
//! on the control plane, tarred it, and streamed the tar into a `busybox`
//! doorstop over `kubectl exec`. ADR-0061's s0 measurement priced that tunnel at
//! **4–15%** of a Step boundary, so deleting it is not where the wall-clock is.
//! The justification is structural:
//!
//! - bulk workspace data no longer crosses the **Kubernetes API server**, a
//!   control-plane component never intended for it;
//! - the control plane no longer buffers and re-writes the whole workspace
//!   through its page cache (s2 measured that tmpdir round-trip *growing* to
//!   ~4.7 s once the CAS leg got faster — it has to be removed, not accelerated);
//! - on the **feed** side, the `exec` tar path could hand a Step a partial tree
//!   and let it run; a failed fetch here fails the init container instead, so the
//!   Step never starts. That is the feed half of git-bug `a3e7845` and only the
//!   feed half — the **drain** kept its own version of the hazard (a truncated
//!   `tar -cf -` unpacking cleanly into a partial tree that then gets published
//!   as an Attempt's authoritative snapshot) long after this binary landed, and
//!   it is closed separately, in the executor's
//!   `exec_capture_stdout`, by framing the captured stream and refusing an
//!   incomplete one. Deleting the feed did not close the class;
//! - and it is the prerequisite for **lazy materialisation** (ADR-0061 part 2),
//!   which *is* the load-bearing part.
//!
//! # Eager, and loudly so
//!
//! This is a **stepping stone**, not an endpoint (ADR-0061 D2.3): it still moves
//! the whole snapshot to every fresh node. The node driver replaces it, and the
//! ticket that closes only when `docker/wsfetch/` is deleted is git-bug
//! `0628369`. So every invocation prints
//! `mode=eager (ADR-0061 s3-feed stepping stone — the node driver replaces this)`
//! into the Step Pod's own logs — a stone you can stand on forever is a floor,
//! and this line is what stops it becoming one silently.
//!
//! # It reuses `materialize`; it does not reimplement it
//!
//! [`WorkspaceClient::materialize`](scarab_workspace_client::WorkspaceClient)
//! already carries the fidelity rules ADR-0061's s7 slice fixed and pinned:
//! unlink-before-write, directories widened for the walk then restored deepest
//! first, mtime-then-mode ordering, symlinks as blobs. Re-implementing any of
//! that here would fork the contract that
//! `crates/scarab-storage-s3/tests/fidelity.rs` and
//! `crates/scarab-workspace-client/tests/service_roundtrip.rs` exist to hold.
//!
//! # Merge-in-order is load-bearing
//!
//! Roots are materialised in the order the executor listed them, so a later
//! input overlays an earlier one (ADR-0007). That is not a detail:
//! `crates/scarab-storage-s3/tests/fidelity.rs::a_later_input_overlays_a_read_only_checkout`
//! exists because a read-only file from input A must be replaceable by input B.
//!
//! # The group-writability contract (git-bug b04697f)
//!
//! Under the ADR-0039 restricted baseline every capability — `DAC_OVERRIDE`
//! included — is dropped, so **group membership is the only thing** that lets a
//! Step write its own workspace. The old feed path ran
//! `chmod -R g+rwX /workspace` + setgid-on-dirs over `exec` after untarring,
//! precisely for that. This binary does the same as a final pass, in-process:
//! see [`widen_for_the_group`].
//!
//! # Fetch exit codes (ticket e140121)
//!
//! The table lives on the constants —
//! [`scarab_workspace_client::EXIT_FETCH_TRANSIENT`] and siblings — shared
//! with the executor so the two sides cannot drift. In short: 0 provisioned,
//! 1 transient-after-retry-window, 2 missing inputs (`NotFound` ONLY — a live
//! Depot said warm + packs + cold all miss), 3 denied (401/403), 4 config
//! (env/skew permanents). Before this ticket, 2 lumped every "permanent"
//! together and every non-zero exit burned the full infra auto-retry budget.
//!
//! # A Depot outage DELAYS a boundary, it never fails it (ADR-0066)
//!
//! Transient failures (connect refused/reset, 5xx, idle-read timeout, a torn
//! read) are retried inside a window proportional to the step's own deadline
//! — `clamp(timeout/10, 5s, 60s)`, timeout via
//! [`scarab_workspace_client::STEP_TIMEOUT_ENV`] — with 1s→10s backoff. One
//! window bounds the WHOLE leg (all roots share the deadline), so a Step with
//! ten inputs does not wait ten windows. The wait bills the step budget
//! (stated in the design; the clock split is a separate ticket).

use std::os::unix::fs::PermissionsExt;

use scarab_storage::{PruneError, StorageError, TreeHash};
use scarab_workspace_client::{
    exclude_paths, DrainErrorKind, DrainErrorRecord, DrainPostOutcome, DrainRecord, IngestReport,
    MemoCas, WorkspaceClient, EXIT_FETCH_CONFIG, EXIT_FETCH_DENIED, EXIT_FETCH_MISSING_INPUTS,
    EXIT_FETCH_TRANSIENT, STEP_TIMEOUT_ENV,
};

/// The env var naming the tmpfs file holding the workspace token. Must agree
/// with `scarab_executor_k8s::workspace_token::WORKSPACE_TOKEN_FILE_ENV`;
/// duplicated as a literal rather than imported because this binary must not
/// link the *kubernetes executor* to read a file (the node driver will reuse
/// this crate too).
const TOKEN_FILE_ENV: &str = "SCARAB_WORKSPACE_TOKEN_FILE";
/// The env var carrying the workspace service's base URL.
const URL_ENV: &str = "SCARAB_WORKSPACE_URL";
/// Comma-separated **Workspace Snapshot** roots, in merge order — the immutable
/// trees this fetcher materialises the mutable Workspace from (CONTEXT.md §4.2).
const ROOTS_ENV: &str = "SCARAB_SNAPSHOT_ROOTS";

/// Cache restore hints (ADR-0065 s1): `dir=root,dir=root`, minted by the
/// executor from the launch-resolved restore pairs. Parseable because
/// pipeline validation forbids `=` and `,` in cache dir names. Mirrors the
/// executor's `WSFETCH_CACHE_ROOTS_ENV` — a wire contract.
const CACHE_ROOTS_ENV: &str = "SCARAB_CACHE_ROOTS";
/// Where to build the Workspace. Overridable only so the binary is testable.
const TARGET_ENV: &str = "SCARAB_WORKSPACE_TARGET";
const DEFAULT_TARGET: &str = "/workspace";

/// Default egress-done marker for `hold`. Must agree with
/// `scarab_executor_k8s`'s `egress_done_marker()` —
/// `{CTL_MOUNT_PATH}/egress-done` with `CTL_MOUNT_PATH = "/scarab-ctl"` —
/// duplicated as a literal for the same reason as [`TOKEN_FILE_ENV`]: this
/// binary must not link the kubernetes executor to poll a file. The executor
/// passes `--marker` explicitly; this default is the skew-safety net.
const DEFAULT_EGRESS_DONE_MARKER: &str = "/scarab-ctl/egress-done";

/// `drain` exit codes (stage-1 contract). 0 = record posted.
const EXIT_DRAIN_TRANSIENT: i32 = 10;
const EXIT_DRAIN_OUTPUT_CONTRACT: i32 = 11;
const EXIT_DRAIN_RECORD_POST: i32 = 12;

/// Cap on the in-process record-POST retry (git-bug afb13c2): comfortably
/// over 3x the Depot's 2 s idle-pack linger — the window a scattered drain's
/// tail packs need to become index-visible — and small next to the control
/// plane's 5-minute drain clock. On exhaustion the exit-12 outer re-drive is
/// the correctness path; this loop only buys back its latency.
const DRAIN_INCOMPLETE_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(10);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // A BARE invocation is `fetch`, and must stay so for skew safety: the
        // fetch init container runs this image with no argv (the image's
        // entrypoint IS the fetcher; the executor overrides nothing), and
        // during a control-plane/image skew an old executor keeps doing that.
        // The explicit word exists so a NEW executor can be unambiguous.
        None | Some("fetch") => {
            let code = match run() {
                Ok(()) => 0,
                Err(FetchError::MissingInputs(msg)) => {
                    eprintln!("scarab-wsfetch: MISSING INPUTS: {msg}");
                    EXIT_FETCH_MISSING_INPUTS
                }
                Err(FetchError::Denied(msg)) => {
                    eprintln!("scarab-wsfetch: DENIED: {msg}");
                    EXIT_FETCH_DENIED
                }
                Err(FetchError::Config(msg)) => {
                    eprintln!("scarab-wsfetch: CONFIG: {msg}");
                    EXIT_FETCH_CONFIG
                }
                Err(FetchError::Transient(msg)) => {
                    eprintln!("scarab-wsfetch: {msg}");
                    EXIT_FETCH_TRANSIENT
                }
            };
            std::process::exit(code);
        }
        Some("hold") => hold(&args[1..]),
        Some("drain") => std::process::exit(drain_main(&args[1..])),
        Some(other) => {
            eprintln!(
                "scarab-wsfetch: unknown subcommand {other:?} — expected `fetch` (default), \
                 `hold`, or `drain`. If the control plane passed this, this image is older \
                 than the executor driving it (image/CP skew)."
            );
            // Config, not the old exit 2: rerunning producers cannot fix skew,
            // and 2 now means "an input snapshot is gone" (ticket e140121).
            std::process::exit(EXIT_FETCH_CONFIG);
        }
    }
}

/// `scarab-wsfetch hold` — the egress doorstop (stage-1 drain).
///
/// Keeps the Pod's egress init container alive across the Step's own
/// termination so the control plane can run the drain inside it, then exits 0
/// when the marker appears. Two properties are the entire job:
///
/// - **SIGTERM is ignored** — `SIG_IGN`, not a handler: there is nothing to
///   do on TERM except NOT die. The busybox `sh` loop this replaces died on
///   TERM, and a hold that dies on TERM silently destroys the drain window
///   (the workspace vanishes with the Pod before the drain ran).
/// - **The marker is the only exit.** No timeout here: the Pod's own
///   `activeDeadlineSeconds` / deletion (SIGKILL) is the backstop, and a
///   second clock in this loop would just race the control plane's.
fn hold(args: &[String]) -> ! {
    let marker = flag_value(args, "--marker")
        .unwrap_or_else(|| DEFAULT_EGRESS_DONE_MARKER.to_string());
    // Safety: single-threaded, before anything else — setting a disposition
    // to SIG_IGN (not a handler fn) is async-signal-trivial.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    println!("scarab-wsfetch: hold — SIGTERM ignored, waiting for {marker}");
    loop {
        if std::path::Path::new(&marker).exists() {
            println!("scarab-wsfetch: hold — {marker} present, releasing");
            std::process::exit(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// The value after `flag` in `args`, if present. No clap: two flags across two
/// subcommands do not buy a dependency in an init-container binary.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

enum FetchError {
    /// An input snapshot is GONE (`StorageError::NotFound` only): a live Depot
    /// answered 404, which means warm, the pack index and the cold archive all
    /// miss it. Retrying the identical spec cannot re-create content — exit
    /// [`EXIT_FETCH_MISSING_INPUTS`], and the recovery is a Rerun/Retry (the
    /// rerun planner widens to the producing steps, ADR-0061 s5).
    MissingInputs(String),
    /// The Depot refused the token (401/403). Retrying with the SAME token
    /// cannot heal — exit [`EXIT_FETCH_DENIED`] immediately; a fresh attempt
    /// mints a fresh fence token.
    Denied(String),
    /// This invocation can never work: required env absent, an address shape
    /// this binary cannot parse — env/image skew, operator-fixable. Exit
    /// [`EXIT_FETCH_CONFIG`].
    Config(String),
    /// Anything else: unreachable service, 5xx, local I/O — exit
    /// [`EXIT_FETCH_TRANSIENT`] (after the retry window, where applicable).
    Transient(String),
}

impl From<std::io::Error> for FetchError {
    fn from(e: std::io::Error) -> Self {
        FetchError::Transient(e.to_string())
    }
}

/// What one [`StorageError`] means for the retry loop — the PURE half of the
/// policy (the loop supplies the clock), so a table test can hold it without
/// scheduling anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryVerdict {
    /// Weather: transport/local-I/O failures (`Backend` — no HTTP answer at
    /// all), retryable statuses (5xx, 429, 408), or a torn read mid-restart
    /// (`HashMismatch` — one re-read is cheap). Retry in the window.
    Retry,
    /// `NotFound` from a live Depot: warm + packs + cold all miss. Permanent.
    MissingInputs,
    /// 401/403 — the same token cannot heal it.
    Denied,
    /// Permanent, non-content: a 4xx the service will answer identically
    /// forever (malformed/contract-violating request — skew or a bug), an
    /// address shape this binary cannot parse (`UnknownAlgorithm`), or an
    /// input nothing can process (`Unsupported`). Retrying only delays the
    /// honest verdict.
    Config,
}

fn retry_verdict(e: &StorageError) -> RetryVerdict {
    match e {
        // `Backend` is TRANSPORT-only since the e140121 review fix (connect
        // refused/reset, idle timeout, local I/O) — always worth the window.
        // `HashMismatch` stays retryable: a torn read mid-restart heals on
        // one re-read, and a persistent mismatch exhausts the window into
        // exit 1 where the bounded re-launch takes over.
        StorageError::Backend(_) | StorageError::HashMismatch => RetryVerdict::Retry,
        // With the status carried, only server weather retries: 5xx (a cold-
        // tier outage surfaces as 5xx, never 404), 429 (asked to back off —
        // backing off IS the loop), 408 (the request died of time, not of
        // shape). Every other 4xx is the service refusing THIS request's
        // form; it will refuse the retry identically, so it is skew/config,
        // not weather.
        StorageError::Status { status, .. } => match status {
            500..=599 | 429 | 408 => RetryVerdict::Retry,
            _ => RetryVerdict::Config,
        },
        StorageError::NotFound => RetryVerdict::MissingInputs,
        StorageError::Denied(_) => RetryVerdict::Denied,
        StorageError::UnknownAlgorithm(_) | StorageError::Unsupported(_) => RetryVerdict::Config,
    }
}

/// The transient-retry window for one whole leg (fetch OR drain):
/// `clamp(step_timeout / 10, 5s, 60s)` — proportional cost (ADR-0066: a Depot
/// outage delays a boundary; the delay must stay a fraction of the budget it
/// bills).
fn retry_window(step_timeout_secs: u64) -> std::time::Duration {
    std::time::Duration::from_secs((step_timeout_secs / 10).clamp(5, 60))
}

/// Parse the executor-stamped step timeout ([`STEP_TIMEOUT_ENV`]). Absent or
/// garbage (image/CP skew — an OLD executor stamps nothing) → 300s, i.e. a
/// 30s window: patient enough to ride out a Depot rollout, small next to any
/// real deadline. Never an error — the timeout only sizes the window.
fn parse_step_timeout_secs(raw: Option<String>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(300)
}

fn step_timeout_secs() -> u64 {
    parse_step_timeout_secs(std::env::var(STEP_TIMEOUT_ENV).ok())
}

/// The backoff pause after `pause`, capped at 10s.
fn next_pause(pause: std::time::Duration) -> std::time::Duration {
    std::cmp::min(pause * 2, std::time::Duration::from_secs(10))
}

/// One leg's transient-retry window, **armed at the first transient failure**
/// — never at leg start (review fix on e140121). Arming at leg start made
/// successful transfer time consume the retry budget: a fetch whose downloads
/// alone take longer than the window would reach its first outage with the
/// window already spent and get ZERO retries — the feature inert exactly for
/// the expensive legs that need it most. Arming on failure keeps the
/// guarantee that matters (at most ONE window of outage-waiting per leg,
/// shared across roots/retries — never N × window) while transfer time stays
/// the step budget's business, not the window's.
struct LegWindow {
    window: std::time::Duration,
    /// The deadline, set by the first transient failure.
    armed: Option<std::time::Instant>,
}

impl LegWindow {
    fn new(window: std::time::Duration) -> Self {
        Self {
            window,
            armed: None,
        }
    }

    /// Called on a transient failure observed at `now`: arms the deadline on
    /// the first call, then answers how long to sleep before the next try —
    /// `pause`, capped to what remains of the window — or `None` once the
    /// window is exhausted.
    fn next_delay(
        &mut self,
        now: std::time::Instant,
        pause: std::time::Duration,
    ) -> Option<std::time::Duration> {
        let deadline = *self.armed.get_or_insert(now + self.window);
        if now >= deadline {
            return None;
        }
        Some(pause.min(deadline - now))
    }
}

/// Parse `SCARAB_SNAPSHOT_ROOTS` into the merge order to materialise in.
///
/// A named function rather than an inline chain in [`run`] specifically so a test
/// can call **this** — the alternative is a test that re-implements the parse and
/// asserts on its own copy, which proves nothing about the binary.
///
/// Order is ADR-0007 semantics, not presentation: the last root to mention a path
/// owns it. Sorting or deduping here would silently hand the overlay to the wrong
/// input. Empties are dropped because the annotation this comes from really does
/// produce trailing commas and spaces.
fn parse_roots(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse `SCARAB_CACHE_ROOTS` (`dir=root,dir=root`) into restore pairs
/// (ADR-0065 s1). Named for the same reason [`parse_roots`] is. Entries
/// without a `=` are dropped loudly-by-shape (an empty result restores
/// nothing, the safe direction); dir names cannot contain `=`/`,` by
/// pipeline validation, so a well-formed producer always round-trips.
fn parse_cache_roots(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .filter_map(|pair| {
            let (dir, root) = pair.split_once('=')?;
            if dir.is_empty() || root.is_empty() {
                return None;
            }
            Some((dir.to_string(), root.to_string()))
        })
        .collect()
}

/// The source-wins backstop (dbe05e5 amendment #2): a cache root restores
/// ONLY into a directory that is absent or empty after the source snapshots
/// materialised. `Ok(true)` = safe to restore; `Ok(false)` = the source
/// produced content here (skip, loudly — validation should have made this
/// unreachable, which is exactly why the runtime refuses to clobber);
/// `Err` = the question could not be answered (a file where a directory was
/// expected, permissions) — the caller skips, loudly, restoring nothing.
fn dir_absent_or_empty(path: &std::path::Path) -> Result<bool, std::io::Error> {
    match std::fs::read_dir(path) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}

fn run() -> Result<(), FetchError> {
    let target = env_or(TARGET_ENV, DEFAULT_TARGET);
    let roots: Vec<String> = parse_roots(&env_or(ROOTS_ENV, ""));
    let cache_roots: Vec<(String, String)> = parse_cache_roots(&env_or(CACHE_ROOTS_ENV, ""));

    // Guard #1 of ADR-0061 D2.3's three anti-calcification guards: this line is
    // printed into every Step Pod's own log, on every Pod, forever — until the
    // node driver deletes this binary. Observable, not silent.
    println!(
        "scarab-wsfetch: mode=eager (ADR-0061 s3-feed stepping stone — the node driver \
         replaces this) inputs={} caches={} target={target}",
        roots.len(),
        cache_roots.len()
    );

    if roots.is_empty() && cache_roots.is_empty() {
        // Nothing to provision. The executor does not schedule this container in
        // that case, so reaching here means a Pod spec drifted — say so and
        // succeed, because an empty workspace is exactly what a no-`needs` Step
        // is entitled to.
        println!("scarab-wsfetch: no input snapshots — leaving the workspace empty");
        return Ok(());
    }

    let base = std::env::var(URL_ENV).map_err(|_| {
        FetchError::Config(format!(
            "{URL_ENV} is not set — this Pod has input snapshots but no workspace service \
             to fetch them from (ADR-0061)"
        ))
    })?;
    let token_file = std::env::var(TOKEN_FILE_ENV).map_err(|_| {
        FetchError::Config(format!(
            "{TOKEN_FILE_ENV} is not set — the workspace token is delivered on tmpfs and is \
             never read from env (ADR-0061)"
        ))
    })?;

    // A multi-thread runtime, not `current_thread`: `materialize` overlaps blob
    // downloads with `spawn_blocking` filesystem writes, and on one thread the
    // two would serialise — which is the property this binary exists to have.
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| FetchError::Transient(format!("tokio runtime: {e}")))?;
    runtime.block_on(fetch(&base, &token_file, &roots, &cache_roots, &target))?;

    widen_for_the_group(std::path::Path::new(&target))?;
    println!("scarab-wsfetch: workspace provisioned at {target}");
    Ok(())
}

/// Apply the executor-stamped transfer byte budget, if any (ticket 16a7768
/// item 1). The executor derives [`scarab_workspace_client::TRANSFER_BYTE_BUDGET_ENV`]
/// from the helper container's memory limit, so the k8s limit and the client's
/// in-flight byte cap move on ONE knob — for the FETCH (downloads) and the
/// DRAIN (uploads) alike, both of which wear the same image and the same
/// limit. Unset/blank = the client's own default; garbage errors with the
/// value named rather than silently running unbudgeted.
fn apply_transfer_budget(client: WorkspaceClient) -> Result<WorkspaceClient, String> {
    match std::env::var(scarab_workspace_client::TRANSFER_BYTE_BUDGET_ENV) {
        Ok(v) if !v.trim().is_empty() => match v.trim().parse::<u64>() {
            Ok(bytes) => Ok(client.with_transfer_byte_budget(bytes)),
            Err(_) => Err(format!(
                "{}={v:?} is not a byte count",
                scarab_workspace_client::TRANSFER_BYTE_BUDGET_ENV
            )),
        },
        _ => Ok(client),
    }
}

async fn fetch(
    base: &str,
    token_file: &str,
    roots: &[String],
    cache_roots: &[(String, String)],
    target: &str,
) -> Result<(), FetchError> {
    let client = WorkspaceClient::from_token_file(base, token_file)
        .map_err(|e| FetchError::Transient(format!("workspace token: {e}")))?;
    // Config, not Transient (review fix): a garbled budget env is stamped by
    // the EXECUTOR — image/executor skew — and re-launching re-stamps the
    // identical garbage, so the infra retry budget cannot heal it.
    let client = apply_transfer_budget(client).map_err(FetchError::Config)?;
    // ONE window for the whole fetch leg (ticket e140121): every root's
    // retries share this deadline, so the worst-case delay is proportional to
    // the step's own budget, not to its input count. Armed at the FIRST
    // transient failure (see [`LegWindow`]) so transfer time never eats the
    // retry budget. Only transient failures (see `retry_verdict`) retry;
    // NotFound/Denied/skew exit immediately.
    let mut win = LegWindow::new(retry_window(step_timeout_secs()));
    for (i, root) in roots.iter().enumerate() {
        let started = std::time::Instant::now();
        let mut pause = std::time::Duration::from_secs(1);
        // Merge-in-order (ADR-0007): later roots overlay earlier ones, so this
        // loop is sequential ON PURPOSE. Parallelising it would make the result
        // depend on which download finished first.
        loop {
            // Whole-ROOT retry: `materialize` is a merge-in-order overlay, so
            // re-running a root over its own partial restore overwrites
            // correctly (idempotent). Re-downloading the root's blobs is the
            // accepted cost — per-blob resume is future perf work.
            let err = match scarab_storage::Cas::materialize(
                &client,
                &TreeHash(root.clone()),
                target,
            )
            .await
            {
                Ok(()) => break,
                Err(e) => e,
            };
            match retry_verdict(&err) {
                RetryVerdict::Retry => {
                    let Some(sleep) = win.next_delay(std::time::Instant::now(), pause) else {
                        return Err(FetchError::Transient(format!(
                            "materialize {root}: {err} — still failing after the \
                             {}s retry window (a Depot outage longer than this \
                             delays the attempt into a bounded re-launch)",
                            win.window.as_secs()
                        )));
                    };
                    eprintln!(
                        "scarab-wsfetch: materialize {root}: {err} — transient, \
                         retrying in {} ms (window {}s)",
                        sleep.as_millis(),
                        win.window.as_secs()
                    );
                    tokio::time::sleep(sleep).await;
                    pause = next_pause(pause);
                }
                RetryVerdict::MissingInputs => {
                    return Err(FetchError::MissingInputs(format!(
                        "input snapshot {root} is not in the workspace service, its pack \
                         index or its cold archive — this Step can never be provisioned \
                         from the identical spec (evicted, or the store was wiped); \
                         Rerun/Retry regenerates it"
                    )))
                }
                RetryVerdict::Denied => {
                    return Err(FetchError::Denied(format!(
                        "materialize {root}: {err} — retrying with the same workspace \
                         token cannot heal a denial; a fresh attempt mints a fresh one"
                    )))
                }
                RetryVerdict::Config => {
                    return Err(FetchError::Config(format!("materialize {root}: {err}")))
                }
            }
        }
        println!(
            "scarab-wsfetch: input {}/{} {} materialised in {} ms",
            i + 1,
            roots.len(),
            root,
            started.elapsed().as_millis()
        );
    }
    // Cache restores (ADR-0065 s1) come strictly AFTER the source snapshots
    // and are tolerant end to end: the required roots above stay fail-closed,
    // while a cache that cannot restore — evicted from warm, on another
    // replica, racing eviction between mint and fetch — is a logged MISS the
    // Step recovers from by rebuilding (slower, never wrong; that property is
    // the licence for evicting a Cache at all).
    for (dir, root) in cache_roots {
        restore_cache(&client, target, dir, root).await;
    }
    Ok(())
}

/// Restore one cache dir, best-effort (ADR-0065 s1). Never returns an error:
/// every outcome is a log line — `hit` (materialised), `miss` (the root could
/// not be fetched; any partial restore is removed so the Step sees
/// absent-not-torn), or `skipped (source wins)` (dbe05e5 amendment #2: the
/// source snapshots already produced a non-empty dir here, and a cache must
/// never clobber source-derived content — validation makes this unreachable
/// for well-formed pipelines, and the backstop holds even when it is not).
async fn restore_cache(client: &WorkspaceClient, target: &str, dir: &str, root: &str) {
    let at = std::path::Path::new(target).join(dir);
    match dir_absent_or_empty(&at) {
        Ok(true) => {}
        Ok(false) => {
            println!(
                "scarab-wsfetch: cache restore skipped (source wins) dir={dir} — \
                 the workspace already has content here"
            );
            return;
        }
        Err(e) => {
            println!(
                "scarab-wsfetch: cache restore skipped dir={dir} — cannot inspect \
                 the target ({e})"
            );
            return;
        }
    }
    let Some(at_str) = at.to_str() else {
        println!("scarab-wsfetch: cache restore skipped dir={dir} — non-UTF-8 target path");
        return;
    };
    let started = std::time::Instant::now();
    match scarab_storage::Cas::materialize(client, &TreeHash(root.to_string()), at_str).await {
        Ok(()) => println!(
            "scarab-wsfetch: cache restore hit dir={dir} root={root} in {} ms",
            started.elapsed().as_millis()
        ),
        Err(e) => {
            // Absent-not-torn: a half-materialised cache dir would be WRONG
            // (a truncated node_modules passes for a whole one); an absent
            // one is only slow. Best-effort removal — a leftover empty dir
            // is harmless.
            let _ = std::fs::remove_dir_all(&at);
            println!("scarab-wsfetch: cache restore miss dir={dir} root={root}: {e}");
        }
    }
}

/// Make the restored workspace **group**-writable, and make directories setgid so
/// files the Step creates stay in that group for the drain.
///
/// Which group is not this process's choice: the snapshot lands owned by whatever
/// gid this container runs as (65532, matching the executor's `WORKSPACE_GID`), and
/// every Step is put in that group via `supplementalGroups`. So widening the group
/// bits is exactly and only what makes the workspace writable.
///
/// This is `chmod -R g+rwX` + `find -type d -exec chmod g+s` — verbatim the two
/// commands the deleted `exec` feed ran, and for verbatim the same reason
/// (git-bug b04697f). A Step runs as whatever uid its image or governance grant
/// dictates, the snapshot arrives owned by *this* process's uid, and the ADR-0039
/// baseline has dropped `DAC_OVERRIDE`. Without this pass the first write in the
/// single most common CI shape — "clone, then build in the workspace" — is
/// `Permission denied`.
///
/// `X` is the standard `chmod` "execute only if it is a directory or already has
/// some execute bit" semantics, so a data file does not become executable.
///
/// Deliberately **after** `materialize`, never interleaved: the mode a snapshot
/// records is the mode `materialize` must restore (ADR-0061 s7 fidelity), and
/// widening is a separate, later, additive act. Symlinks are skipped — a link's
/// own mode is meaningless and `chmod` would follow it out of the workspace.
fn widen_for_the_group(root: &std::path::Path) -> Result<(), std::io::Error> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let is_dir = meta.is_dir();
        let mode = meta.permissions().mode() & 0o7777;
        let mut widened = mode | 0o060; // g+rw
        if is_dir || mode & 0o111 != 0 {
            widened |= 0o010; // g+X
        }
        if is_dir {
            widened |= 0o2000; // g+s — the drain must see the group survive
        }
        if widened != mode {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(widened))?;
        }
        if is_dir {
            for entry in std::fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        }
    }
    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

// ---------------------------------------------------------------------------
// drain — the in-Pod half of the stage-1 drain
// ---------------------------------------------------------------------------

/// `scarab-wsfetch drain --workspace /workspace [--outputs <path>]…`
///
/// Ingest → prune+identity in-process → POST the [`DrainRecord`] LAST. The
/// record is the rendezvous: the control plane classifies record-first, so
/// nothing here is authoritative except what lands on the Depot — the exit
/// code is a hint (`0` posted, `10` transient, `11` output contract, `12`
/// record POST failed after a successful ingest).
///
/// Depot URL and token file come from the same envs `fetch` uses
/// ([`URL_ENV`], [`TOKEN_FILE_ENV`]): same Pod, same tmpfs Secret, same
/// service — only the direction differs.
fn drain_main(args: &[String]) -> i32 {
    let mut workspace = env_or(TARGET_ENV, DEFAULT_TARGET);
    let mut outputs: Vec<String> = Vec::new();
    let mut cache_dirs: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--workspace" => match it.next() {
                Some(v) => workspace = v.clone(),
                None => {
                    eprintln!("scarab-wsfetch: drain: --workspace needs a value");
                    return EXIT_DRAIN_TRANSIENT;
                }
            },
            "--outputs" => match it.next() {
                Some(v) => outputs.push(v.clone()),
                None => {
                    eprintln!("scarab-wsfetch: drain: --outputs needs a value");
                    return EXIT_DRAIN_TRANSIENT;
                }
            },
            "--cache-dirs" => match it.next() {
                Some(v) => cache_dirs.push(v.clone()),
                None => {
                    eprintln!("scarab-wsfetch: drain: --cache-dirs needs a value");
                    return EXIT_DRAIN_TRANSIENT;
                }
            },
            other => {
                // A flag this binary does not know means the control plane is
                // newer than this image. Transient (the CP's 5-min clock
                // bounds it), and the message names the real cause.
                eprintln!(
                    "scarab-wsfetch: drain: unrecognised argument {other:?} — probable \
                     image/CP skew (this image is older than the executor driving it)"
                );
                return EXIT_DRAIN_TRANSIENT;
            }
        }
    }
    let base = match std::env::var(URL_ENV) {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("scarab-wsfetch: drain: {URL_ENV} is not set");
            return EXIT_DRAIN_TRANSIENT;
        }
    };
    let token_file = match std::env::var(TOKEN_FILE_ENV) {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("scarab-wsfetch: drain: {TOKEN_FILE_ENV} is not set");
            return EXIT_DRAIN_TRANSIENT;
        }
    };
    let client = match WorkspaceClient::from_token_file(&base, &token_file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("scarab-wsfetch: drain: workspace token: {e}");
            return EXIT_DRAIN_TRANSIENT;
        }
    };
    let client = match apply_transfer_budget(client) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("scarab-wsfetch: drain: {e}");
            return EXIT_DRAIN_TRANSIENT;
        }
    };
    // Multi-thread for the same reason as `fetch`: the ingest overlaps
    // hashing/reading with uploads.
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("scarab-wsfetch: drain: tokio runtime: {e}");
            return EXIT_DRAIN_TRANSIENT;
        }
    };
    runtime.block_on(run_drain(&client, &workspace, &outputs, &cache_dirs))
}

async fn run_drain(
    client: &WorkspaceClient,
    workspace: &str,
    outputs: &[String],
    cache_dirs: &[String],
) -> i32 {
    let t_ingest = std::time::Instant::now();
    // EACCES/EPERM anywhere in the walk surfaces HERE as a hard error — the
    // scan propagates every read_dir/read/metadata failure, it never skips a
    // file it cannot read (a skipped file would publish a silently narrower
    // snapshot as the Attempt's authoritative evidence).
    //
    // The DRAIN variant: trees are PUT unconditionally so every tree of the
    // closure enters this fence's write ledger (only a PUT appends it), or
    // the record POST below would 422 on any incremental workspace.
    //
    // WHOLE-LEG retry (ticket e140121 F6): a transient failure re-runs
    // `drain_ingest_report` from the top inside the same window the fetch leg
    // uses — never per-op retries inside the client lib (the lib is shared
    // with the control plane, whose TieredCas has its own fallback policy).
    // From-scratch is idempotent by construction: the re-scan is local, the
    // batched `/have` keys durable content on the pack index so bytes a Depot
    // restart lost re-upload while surviving packs dedup, a PUT of identical
    // bytes is an idempotent overwrite, and the write-ledger append upserts
    // (`ON CONFLICT DO UPDATE` on `depot_fence_writes`).
    // Armed at the first transient failure, like the fetch leg's — hashing
    // and uploading the workspace can alone outlast the window, and a window
    // armed at leg start would leave such a drain ZERO retries.
    let mut win = LegWindow::new(retry_window(step_timeout_secs()));
    let mut ingest_pause = std::time::Duration::from_secs(1);
    let report = loop {
        let err = match client.drain_ingest_report(workspace, outputs, cache_dirs).await {
            Ok(r) => break r,
            Err(e) => e,
        };
        if retry_verdict(&err) != RetryVerdict::Retry {
            // Non-transient (denied / a permanent 4xx / an unscannable
            // workspace): exit 10 immediately — classification is
            // record-first on the control plane, and its bounded outer
            // re-drive stays the correctness path.
            eprintln!("scarab-wsfetch: drain: ingest {workspace}: {err}");
            return EXIT_DRAIN_TRANSIENT;
        }
        let Some(sleep) = win.next_delay(std::time::Instant::now(), ingest_pause) else {
            // Window exhausted: same exit, the outer re-drive takes over.
            eprintln!("scarab-wsfetch: drain: ingest {workspace}: {err}");
            return EXIT_DRAIN_TRANSIENT;
        };
        eprintln!(
            "scarab-wsfetch: drain: ingest {workspace}: {err} — transient, retrying \
             the whole leg in {} ms (window {}s)",
            sleep.as_millis(),
            win.window.as_secs()
        );
        tokio::time::sleep(sleep).await;
        ingest_pause = next_pause(ingest_pause);
    };
    let ingest_ms = t_ingest.elapsed().as_millis() as u64;
    let IngestReport {
        snapshot,
        trees,
        files,
        tree_bytes,
        blobs_uploaded,
        bytes_uploaded,
        have_hits,
        cache_roots,
    } = report;
    println!(
        "scarab-wsfetch: drain — ingested {workspace}: root={} files={files} \
         blobs_uploaded={blobs_uploaded} bytes_uploaded={bytes_uploaded} \
         have_hits={have_hits} ingest_ms={ingest_ms} cache_saved={}",
        snapshot.root.0,
        cache_roots.len()
    );

    // Prune + identity IN-PROCESS: tree read-backs come from the scan's own
    // canonical bytes via `MemoCas` — zero HTTP tree GETs on the hot path.
    // The prune-minted trees are real writes through the client, so the
    // Depot's warm tier and this fence's write ledger hold everything the
    // posted record names.
    let memo = MemoCas::new(client, trees);
    let t_prune = std::time::Instant::now();
    let (pruned_root, identity) = if outputs.is_empty() && !cache_dirs.is_empty() {
        // No declared outputs, but cache dirs (ADR-0065 s1): the published
        // root is the full snapshot with the cache dirs EXCLUDED — a cached
        // dir flows via the cache, not via the workspace. The exclusion runs
        // over the memo, so the minted parents are real writes: warm and this
        // fence's write ledger hold everything the record names (that is
        // what lets `pruned_root` validate).
        let excluded = match exclude_paths(&memo, &snapshot.root, cache_dirs).await {
            Ok(excluded) => excluded,
            Err(e) => {
                eprintln!("scarab-wsfetch: drain: cache exclusion: {e}");
                return EXIT_DRAIN_TRANSIENT;
            }
        };
        if excluded == snapshot.root {
            // No declared dir was actually present — nothing was excluded.
            (None, snapshot.identity.as_ref().map(|t| t.0.clone()))
        } else {
            let identity = match scarab_storage::content_identity(&memo, &excluded).await {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("scarab-wsfetch: drain: excluded identity: {e}");
                    return EXIT_DRAIN_TRANSIENT;
                }
            };
            (Some(excluded.0), Some(identity.0))
        }
    } else if outputs.is_empty() {
        // `ingest` folded the identity for free; nothing to walk.
        (None, snapshot.identity.as_ref().map(|t| t.0.clone()))
    } else {
        let pruned = match scarab_storage::prune_tree(&memo, &snapshot.root, outputs).await {
            Ok(pruned) => pruned,
            Err(e) => match classify_prune(e, outputs) {
                PruneVerdict::Transient(detail) => {
                    eprintln!("scarab-wsfetch: drain: {detail}");
                    return EXIT_DRAIN_TRANSIENT;
                }
                PruneVerdict::OutputContract(detail) => {
                    // Post the error record best-effort, then exit 11 either
                    // way: the CP is record-first, and its no-record +
                    // exit-hint-11 arm is the designed fallback when this
                    // POST cannot land.
                    let rec = DrainRecord {
                        root: snapshot.root.0.clone(),
                        pruned_root: None,
                        identity: None,
                        files,
                        tree_bytes,
                        blobs_uploaded,
                        bytes_uploaded,
                        have_hits,
                        ingest_ms,
                        prune_ms: t_prune.elapsed().as_millis() as u64,
                        // An error record publishes nothing — no saves either.
                        cache_roots: Default::default(),
                        error: Some(DrainErrorRecord {
                            kind: DrainErrorKind::OutputContract,
                            detail: detail.clone(),
                        }),
                    };
                    if let Err(e) = client.post_drain_record(&rec).await {
                        eprintln!(
                            "scarab-wsfetch: drain: OutputContract error record POST failed \
                             (the exit code carries the verdict instead): {e}"
                        );
                    }
                    eprintln!("scarab-wsfetch: drain: OUTPUT CONTRACT: {detail}");
                    return EXIT_DRAIN_OUTPUT_CONTRACT;
                }
            },
        };
        // A pruned root is a different tree, so its identity has to be walked
        // — over the memo, where every tree it can name already sits.
        let identity = match scarab_storage::content_identity(&memo, &pruned).await {
            Ok(id) => id,
            Err(e) => {
                eprintln!("scarab-wsfetch: drain: outputs identity: {e}");
                return EXIT_DRAIN_TRANSIENT;
            }
        };
        (Some(pruned.0), Some(identity.0))
    };
    let prune_ms = t_prune.elapsed().as_millis() as u64;

    // The record goes LAST: by the time the Depot validates it, every address
    // it names is already in warm and in this fence's ledger.
    let rec = DrainRecord {
        root: snapshot.root.0.clone(),
        pruned_root: pruned_root.clone(),
        identity,
        files,
        tree_bytes,
        blobs_uploaded,
        bytes_uploaded,
        have_hits,
        ingest_ms,
        prune_ms,
        // The saves (ADR-0065 s1): declared dir → its subtree root, present
        // dirs only. Every one of these trees was PUT by this drain (the
        // drain PUTs all scan trees unconditionally), so they are in this
        // fence's write ledger by construction — the Depot 422s a record
        // naming one that is not.
        cache_roots,
        error: None,
    };
    // Retried in-process ONLY on the Depot's machine-readable
    // `drain_state_incomplete` 422 (git-bug afb13c2): a scattered drain's
    // members can sit in another replica's open tail pack until that
    // replica's idle linger seals it into the shared index, and every retry
    // sees a strictly larger index. Any other failure exits 12 immediately —
    // the outer re-drive is the correctness path either way.
    let mut waited = std::time::Duration::ZERO;
    let mut pause = std::time::Duration::from_millis(500);
    loop {
        match client.post_drain_record_classified(&rec).await {
            Ok(DrainPostOutcome::Posted) => {
                println!(
                    "scarab-wsfetch: drain — record posted: root={} pruned_root={} prune_ms={prune_ms}",
                    rec.root,
                    pruned_root.as_deref().unwrap_or("-"),
                );
                return 0;
            }
            // 409: a success record already stands under this fence — ours by
            // construction (the fence key includes the attempt), i.e. an
            // earlier POST landed but its 2xx was lost. The record is
            // deposited; exit 0 for exit-code honesty (ticket e140121 F5).
            Ok(DrainPostOutcome::AlreadyPosted) => {
                println!(
                    "scarab-wsfetch: drain — record was ALREADY posted under this fence \
                     (a lost 2xx retried into 409): root={}",
                    rec.root,
                );
                return 0;
            }
            Ok(DrainPostOutcome::StateIncomplete(detail)) => {
                if waited >= DRAIN_INCOMPLETE_RETRY_CAP {
                    eprintln!(
                        "scarab-wsfetch: drain: record POST still incomplete after {}ms — \
                         handing over to the outer re-drive: {detail}",
                        waited.as_millis()
                    );
                    return EXIT_DRAIN_RECORD_POST;
                }
                eprintln!(
                    "scarab-wsfetch: drain: drain state incomplete (a tail pack may still \
                     be inside another replica's linger window) — retrying in {}ms: {detail}",
                    pause.as_millis()
                );
                tokio::time::sleep(pause).await;
                waited += pause;
                pause = std::cmp::min(pause * 2, std::time::Duration::from_millis(2_500));
            }
            Err(e) => {
                eprintln!(
                    "scarab-wsfetch: drain: record POST failed after successful ingest \
                     (the snapshot is in warm; only the rendezvous is missing): {e}"
                );
                return EXIT_DRAIN_RECORD_POST;
            }
        }
    }
}

/// How one [`PruneError`] maps onto the drain's exit codes.
enum PruneVerdict {
    /// Exit 10 — the walk could not be performed (storage/transport).
    Transient(String),
    /// Exit 11 — the walk was performed and the declaration is unsatisfiable.
    OutputContract(String),
}

/// Mirrors `drive_workspace`'s prune arms EXACTLY (the `outputs:` leg of
/// `crates/scarab-executor-k8s/src/lib.rs`, the two `match` arms on
/// `prune_tree`'s error): `PruneError::Storage` → the transient
/// `drain_failure` class, with the same `prune outputs: …` prefix;
/// `MissingPath`/`UnsafePath` → `DriveErr::OutputContract`, with the same
/// `outputs: … (declared: …)` sentence — so the operator reads identical
/// wording whichever side classified it.
fn classify_prune(err: PruneError, declared: &[String]) -> PruneVerdict {
    match err {
        PruneError::Storage(e) => PruneVerdict::Transient(format!("prune outputs: {e}")),
        permanent => PruneVerdict::OutputContract(format!(
            "outputs: {permanent} (declared: {})",
            declared.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The retry policy at the fn grain (ticket e140121; repo rule: construct
    /// the decision, don't schedule it). The table IS the contract: weather
    /// retries — transport, 5xx, 429, 408, a torn read — and NOTHING else:
    /// absence is MissingInputs, a denial is Denied, a permanent 4xx / an
    /// unparseable address / an unscannable input is Config. Mutations killed
    /// in each direction — retrying NotFound would grind an evicted snapshot
    /// against the window and then burn 3 attempts; NOT retrying transport or
    /// 5xx re-opens the ticket's bug (a 20s Depot restart dead-lettering
    /// runs); retrying a 400/405/409/422 (review finding) grinds the whole
    /// window and 3 Infra attempts on a request the service refuses
    /// identically forever.
    #[test]
    fn retry_verdict_retries_weather_and_only_weather() {
        let status = |status: u16| StorageError::Status {
            status,
            body: "x".into(),
        };
        for (err, want) in [
            // Transport: no HTTP answer at all.
            (
                StorageError::Backend("workspace service unreachable: connect refused".into()),
                RetryVerdict::Retry,
            ),
            // Server weather, status-carried.
            (status(500), RetryVerdict::Retry),
            (status(503), RetryVerdict::Retry),
            (status(429), RetryVerdict::Retry),
            (status(408), RetryVerdict::Retry),
            // A torn read mid-restart: one re-read is cheap.
            (StorageError::HashMismatch, RetryVerdict::Retry),
            (StorageError::NotFound, RetryVerdict::MissingInputs),
            (
                StorageError::Denied("workspace service 401 Unauthorized: expired".into()),
                RetryVerdict::Denied,
            ),
            // Permanent 4xx: the service refuses THIS request's shape, and
            // will refuse the retry identically — skew/config, not weather.
            (status(400), RetryVerdict::Config),
            (status(405), RetryVerdict::Config),
            (status(409), RetryVerdict::Config),
            (status(422), RetryVerdict::Config),
            (
                StorageError::UnknownAlgorithm("blake3".into()),
                RetryVerdict::Config,
            ),
            // An input nothing can process (a FIFO in the workspace): the
            // re-scan re-refuses identically — never worth the window.
            (
                StorageError::Unsupported("refusing to snapshot a FIFO".into()),
                RetryVerdict::Config,
            ),
        ] {
            assert_eq!(retry_verdict(&err), want, "{err:?}");
        }
    }

    /// `clamp(timeout/10, 5s, 60s)`: the floor keeps a tiny timeout from
    /// producing a useless sub-second window, the ceiling keeps a 23h step
    /// from stalling Init for over two hours, and in between the cost is
    /// proportional (a 300s default-when-unstamped step waits ≤30s).
    #[test]
    fn the_retry_window_is_a_clamped_tenth_of_the_step_timeout() {
        use std::time::Duration;
        assert_eq!(retry_window(1), Duration::from_secs(5), "floor");
        assert_eq!(retry_window(50), Duration::from_secs(5), "floor edge");
        assert_eq!(retry_window(300), Duration::from_secs(30), "proportional");
        assert_eq!(retry_window(600), Duration::from_secs(60), "ceiling edge");
        assert_eq!(retry_window(23 * 3600), Duration::from_secs(60), "ceiling");
    }

    /// The env parse is skew-tolerant BY CONTRACT: an old executor stamps
    /// nothing and a garbled value must not fail the fetch (the timeout only
    /// sizes the window) — both fall back to 300s, i.e. a 30s window.
    #[test]
    fn a_missing_or_garbled_step_timeout_defaults_to_300s() {
        assert_eq!(parse_step_timeout_secs(None), 300);
        assert_eq!(parse_step_timeout_secs(Some("".into())), 300);
        assert_eq!(parse_step_timeout_secs(Some("not-a-number".into())), 300);
        assert_eq!(parse_step_timeout_secs(Some("0".into())), 300);
        assert_eq!(parse_step_timeout_secs(Some(" 7200 ".into())), 7200);
    }

    /// Backoff doubles and caps at 10s — the cap is what keeps a 60s window
    /// useful (1+2+4+8+10+10+… tries ~7 times, not 3).
    #[test]
    fn backoff_doubles_to_a_10s_cap() {
        use std::time::Duration;
        assert_eq!(next_pause(Duration::from_secs(1)), Duration::from_secs(2));
        assert_eq!(next_pause(Duration::from_secs(4)), Duration::from_secs(8));
        assert_eq!(next_pause(Duration::from_secs(8)), Duration::from_secs(10));
        assert_eq!(next_pause(Duration::from_secs(10)), Duration::from_secs(10));
    }

    /// The window arms at the FIRST transient failure, never at leg start
    /// (review fix): a leg whose successful transfers alone outlast the whole
    /// window still gets its full retry budget when the outage finally hits.
    /// Mutation killed: reverting to `deadline = leg_start + window`, which
    /// made the feature inert for exactly the expensive fetches that need it.
    #[test]
    fn the_window_arms_at_the_first_failure_not_at_leg_start() {
        use std::time::{Duration, Instant};
        let mut win = LegWindow::new(Duration::from_secs(5));
        let leg_start = Instant::now();
        // Transfers ran 100s — 20x the window — before the first failure…
        let first_failure = leg_start + Duration::from_secs(100);
        // …and the first transient still gets its full pause: the window is
        // only NOW being armed.
        assert_eq!(
            win.next_delay(first_failure, Duration::from_secs(1)),
            Some(Duration::from_secs(1)),
            "transfer time must not consume the retry budget"
        );
        // Inside the armed window the sleep is capped to what remains…
        assert_eq!(
            win.next_delay(first_failure + Duration::from_secs(4), Duration::from_secs(10)),
            Some(Duration::from_secs(1)),
            "the sleep never overshoots the armed deadline"
        );
        // …and once the armed deadline passes, the window is exhausted. Still
        // ONE window per leg: arming happened exactly once.
        assert_eq!(
            win.next_delay(first_failure + Duration::from_secs(5), Duration::from_secs(1)),
            None
        );
        assert_eq!(
            win.next_delay(first_failure + Duration::from_secs(60), Duration::from_secs(1)),
            None,
            "a later failure must not re-arm a second window"
        );
    }

    /// The b04697f contract, at the grain the fetcher owns it: after the pass a
    /// group member can read, write and traverse everything, directories are
    /// setgid, a data file does NOT become executable, and an executable stays
    /// executable.
    #[test]
    fn widening_grants_the_group_exactly_what_a_step_needs() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("dist/nested")).expect("dirs");
        std::fs::write(root.join("dist/data.txt"), b"x").expect("file");
        std::fs::write(root.join("dist/run.sh"), b"#!/bin/sh\n").expect("file");
        // A read-only file and a locked-down directory — what a checkout of a
        // vendored dependency tree actually looks like.
        std::fs::set_permissions(
            &root.join("dist/data.txt"),
            std::fs::Permissions::from_mode(0o400),
        )
        .expect("chmod");
        std::fs::set_permissions(
            &root.join("dist/run.sh"),
            std::fs::Permissions::from_mode(0o500),
        )
        .expect("chmod");
        std::fs::set_permissions(
            &root.join("dist/nested"),
            std::fs::Permissions::from_mode(0o500),
        )
        .expect("chmod");

        widen_for_the_group(root).expect("widen");

        let mode = |p: &str| {
            std::fs::metadata(root.join(p))
                .expect(p)
                .permissions()
                .mode()
                & 0o7777
        };
        assert_eq!(mode("dist/data.txt") & 0o070, 0o060, "g+rw, and NOT g+x");
        assert_eq!(mode("dist/run.sh") & 0o070, 0o070, "already +x ⇒ g+x too");
        assert_eq!(mode("dist/nested") & 0o070, 0o070, "a dir is always g+x");
        assert_eq!(mode("dist/nested") & 0o2000, 0o2000, "and setgid");
        assert_eq!(mode("dist") & 0o2000, 0o2000);
        // The owner's bits are never taken away.
        assert_eq!(mode("dist/data.txt") & 0o700, 0o400);
    }

    /// A symlink must not be followed: chmod-ing through one would change a file
    /// outside the workspace, and a link's own mode means nothing.
    #[test]
    fn widening_does_not_follow_symlinks() {
        let tmp = tempfile::tempdir().expect("tmp");
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"secret").expect("file");
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o400)).expect("chmod");

        let root = tmp.path().join("ws");
        std::fs::create_dir(&root).expect("ws");
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("symlink");

        widen_for_the_group(&root).expect("widen");
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o7777,
            0o400,
            "the symlink's target must be untouched"
        );
    }

    /// The `dir=root,…` parse (ADR-0065 s1): well-formed pairs round-trip in
    /// order; malformed fragments (no `=`, empty halves, empties from
    /// trailing commas) drop rather than restore something half-named.
    #[test]
    fn cache_roots_parse_pairs_and_drop_malformed_fragments() {
        assert_eq!(
            parse_cache_roots("node_modules=abc, .cargo=def ,"),
            vec![
                ("node_modules".to_string(), "abc".to_string()),
                (".cargo".to_string(), "def".to_string()),
            ]
        );
        assert!(parse_cache_roots("").is_empty());
        assert!(parse_cache_roots("no-separator").is_empty());
        assert!(parse_cache_roots("=root").is_empty());
        assert!(parse_cache_roots("dir=").is_empty());
    }

    /// The source-wins backstop's decision (dbe05e5 amendment #2): absent and
    /// empty dirs restore; a dir the source populated does not; a FILE where
    /// the dir would go is an `Err` (the caller skips loudly).
    #[test]
    fn source_wins_decision_restores_only_into_absent_or_empty_dirs() {
        let tmp = tempfile::tempdir().expect("tmp");
        assert!(dir_absent_or_empty(&tmp.path().join("absent")).expect("absent"));
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).expect("mkdir");
        assert!(dir_absent_or_empty(&empty).expect("empty"));
        let full = tmp.path().join("full");
        std::fs::create_dir(&full).expect("mkdir");
        std::fs::write(full.join("index.js"), b"source").expect("file");
        assert!(!dir_absent_or_empty(&full).expect("full"));
        let file = tmp.path().join("file");
        std::fs::write(&file, b"a file, not a dir").expect("file");
        assert!(dir_absent_or_empty(&file).is_err());
    }

    /// The backstop end to end (dbe05e5 amendment #2): a cache restore into a
    /// dir the source populated is a SKIP that touches neither the dir's
    /// content nor the network — the client below points at nothing routable,
    /// so reaching for it would fail the test by timeout/panic rather than
    /// silently clobbering. And a restore whose fetch fails (same dead
    /// client, absent dir) leaves the dir ABSENT — miss, not torn.
    #[tokio::test]
    async fn restore_never_clobbers_source_content_and_a_failed_restore_leaves_absence() {
        let dead = WorkspaceClient::new("http://127.0.0.1:1", "unused-token");
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join("node_modules")).expect("mkdir");
        std::fs::write(ws.join("node_modules/index.js"), b"from the source").expect("file");

        let target = ws.to_str().expect("utf-8");
        restore_cache(&dead, target, "node_modules", &"a".repeat(64)).await;
        assert_eq!(
            std::fs::read(ws.join("node_modules/index.js")).expect("still there"),
            b"from the source",
            "source wins — the cache must not clobber it"
        );

        restore_cache(&dead, target, "vendor", &"b".repeat(64)).await;
        assert!(
            !ws.join("vendor").exists(),
            "a failed restore leaves the dir absent (miss), never torn"
        );
    }

    /// The drain's exit-11 classification, at the fn grain (the process exit
    /// is just `match` + `return`): a declared path the step did not produce
    /// — and an unsafe one — are OutputContract; a storage failure is
    /// transient. Mirrors the CP's arms in `drive_workspace`.
    ///
    /// Mutation killed: swapping the arms. Transient-for-MissingPath would
    /// re-drive a permanent contract violation until the 5-min clock dead-
    /// letters it as Transient (wrong verdict, wrong operator story);
    /// OutputContract-for-Storage would turn a Depot blip into a permanent
    /// config failure of the Attempt.
    #[test]
    fn a_missing_declared_output_is_a_contract_violation_and_a_storage_error_is_not() {
        let declared = vec!["dist".to_string(), "report.xml".to_string()];
        match classify_prune(PruneError::MissingPath("dist".into()), &declared) {
            PruneVerdict::OutputContract(detail) => {
                // The CP's exact sentence shape, so both sides read the same.
                assert!(detail.starts_with("outputs: "), "{detail}");
                assert!(detail.contains("dist"), "{detail}");
                assert!(detail.contains("(declared: dist, report.xml)"), "{detail}");
            }
            PruneVerdict::Transient(d) => panic!("MissingPath must be OutputContract, got Transient({d})"),
        }
        match classify_prune(PruneError::UnsafePath("../escape".into()), &declared) {
            PruneVerdict::OutputContract(_) => {}
            PruneVerdict::Transient(d) => panic!("UnsafePath must be OutputContract, got Transient({d})"),
        }
        match classify_prune(
            PruneError::Storage(StorageError::Backend("connection refused".into())),
            &declared,
        ) {
            PruneVerdict::Transient(detail) => {
                assert!(detail.starts_with("prune outputs: "), "{detail}");
            }
            PruneVerdict::OutputContract(d) => {
                panic!("a storage failure must be Transient, got OutputContract({d})")
            }
        }
    }

    /// The hold marker default must be the executor's `egress_done_marker()`
    /// literal — `--marker` overrides it, absence falls back. Honestly: this
    /// test pins only THIS binary's copy of the literal; it kills a typo in
    /// the duplicated path constant only in tandem with the executor-side
    /// test that pins `/scarab-ctl/egress-done` against `egress_done_marker()`
    /// (`crates/scarab-executor-k8s/src/lib.rs`). Either literal drifting
    /// alone fails its own side's assert; the PAIR is what guarantees a
    /// skew-window hold does not wait forever on a file the CP never touches.
    #[test]
    fn the_hold_marker_defaults_to_the_executors_egress_done_path() {
        assert_eq!(DEFAULT_EGRESS_DONE_MARKER, "/scarab-ctl/egress-done");
        let args = vec!["--marker".to_string(), "/tmp/other".to_string()];
        assert_eq!(flag_value(&args, "--marker").as_deref(), Some("/tmp/other"));
        assert_eq!(flag_value(&[], "--marker"), None);
    }

    /// The roots parse is merge-ORDER-preserving and tolerant of the shapes the
    /// annotation actually produces (trailing commas, spaces, an empty value).
    ///
    /// This calls [`parse_roots`], the function [`run`] calls. It used to declare
    /// a closure that re-implemented the same chain and assert on *that*, so the
    /// production parse was never executed and could have sorted, deduped or
    /// reversed without turning this red.
    #[test]
    fn inputs_parse_preserves_order_and_drops_empties() {
        // Descending, so a parse that sorted would be caught rather than
        // accidentally agreeing with the input order.
        assert_eq!(parse_roots("b,a"), vec!["b".to_string(), "a".to_string()]);
        assert_eq!(parse_roots(""), Vec::<String>::new());
        assert_eq!(
            parse_roots(" b , ,a,"),
            vec!["b".to_string(), "a".to_string()]
        );
        // Merge order is ADR-0007 semantics: the LAST root to mention a path owns
        // it, so a repeated root is not a duplicate to collapse — deduping "a" out
        // of `a,b,a` would hand the overlay to `b` instead.
        assert_eq!(
            parse_roots("a,b,a"),
            vec!["a".to_string(), "b".to_string(), "a".to_string()]
        );
    }
}
