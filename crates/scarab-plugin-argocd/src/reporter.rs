//! The reporter loop: Argo CD in, DeploymentStatusReports out, following
//! the DeploymentReporterContract (ADR-0070 §11).
//!
//! One **session** is: who am I (`/whoami`) → seed every cursor
//! (`/targets`, all pages) → list Applications → watch them (NDJSON, from
//! the list's `resourceVersion`). Anything that breaks a session — a cut
//! stream, a 401/403, the broker unreachable — ends it, and the next one
//! starts after a backoff with a fresh seed and list, so nothing missed
//! while disconnected is lost and nothing already reported is repeated.
//!
//! Per Application, changes are coalesced for [`Config::debounce`]; then,
//! only if its target revision or last reconcile moved, its managed
//! resources are re-read for the version (ADR-0070 §4), and it is assessed.
//! A report goes out only when *(version, status, label)* differs from the
//! last one — transitions only, never per reconcile.
//!
//! Reports leave in one queue, in order, one batch at a time (a second
//! batch in flight is a 429). Each carries the target's next sequence and a
//! fresh idempotency key; a batch that got no answer is resent unchanged,
//! so a retry is a `duplicate`, never a second fact.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::argo::{Application, ApplicationList, ManagedResources, StreamLine};
use crate::assess::Status;
use crate::broker::{encode_component, Broker, BrokerError};
use crate::config::Config;
use crate::report::{
    conflicting_key, due, observe, ownership_item, status_item, Cursor, Due, KeyMinter,
    Observation, ReportItem, Reported, Unreportable, MAX_ITEMS_PER_BATCH, MAX_SEQUENCE,
};
use crate::version::target_images;

/// The capability every call of this reporter needs (ADR-0072 §2).
const REPORT_DEPLOYMENTS: &str = "report_deployments";
/// Buffered Argo stream lines between the reader task and the loop.
const STREAM_CHANNEL: usize = 256;
/// The longest NDJSON line accepted (one Application object).
const MAX_STREAM_LINE_BYTES: usize = 32 * 1024 * 1024;
/// `Retry-After` when a 429/503 has none: the broker's own defaults.
const DEFAULT_RETRY_AFTER_429: Duration = Duration::from_secs(1);
const DEFAULT_RETRY_AFTER_503: Duration = Duration::from_secs(5);

/// Run until `stop` turns true. Errors are logged and retried with backoff;
/// only the configuration itself can make this return an error.
pub async fn run(cfg: Config, mut stop: watch::Receiver<bool>) -> Result<(), String> {
    let mut reporter = Reporter::new(cfg)?;
    let mut backoff = Backoff::new(reporter.cfg.backoff_min, reporter.cfg.backoff_max);
    loop {
        let started = Instant::now();
        match reporter.session(&mut stop).await {
            Ok(()) => {}
            Err(SessionEnd::Stopped) => {}
            Err(SessionEnd::Reconnect(why)) => {
                tracing::info!(%why, "argocd: session ended; reconnecting");
            }
            Err(SessionEnd::Failed(why)) => {
                tracing::warn!(%why, "argocd: session failed; backing off");
            }
        }
        if stopped(&stop) {
            reporter.last_flush().await;
            tracing::info!("argocd: stopped");
            return Ok(());
        }
        // A session that ran a while was healthy: start the backoff over.
        if started.elapsed() > reporter.cfg.backoff_max {
            backoff.reset();
        }
        if sleep_or_stop(backoff.next(), &mut stop).await {
            reporter.last_flush().await;
            tracing::info!("argocd: stopped");
            return Ok(());
        }
    }
}

/// Why a session ended.
#[derive(Debug)]
enum SessionEnd {
    Stopped,
    /// The watch ended (cut, idle, closed): reconnect.
    Reconnect(String),
    /// A call failed: back off, then reconnect.
    Failed(String),
}

impl From<BrokerError> for SessionEnd {
    fn from(e: BrokerError) -> Self {
        SessionEnd::Failed(e.to_string())
    }
}

/// What the reporter knows of one Application.
struct AppView {
    app: Application,
    /// [`Application::manifests_fingerprint`] when `images` was read.
    fingerprint: String,
    /// Its target pod-template images; `None` until read.
    images: Option<BTreeSet<String>>,
}

/// One line from the watch-stream reader.
enum StreamMsg {
    Line(StreamLine),
}

struct Reporter {
    cfg: Config,
    broker: Broker,
    keys: KeyMinter,
    /// The installation's org, from `/whoami`: owners outside it are
    /// dropped before sending.
    org: String,
    /// Per DeploymentTarget id.
    cursors: HashMap<String, Cursor>,
    /// Per DeploymentTarget id.
    apps: HashMap<String, AppView>,
    /// Reports not yet answered, in source order.
    outbox: VecDeque<ReportItem>,
    /// A `stale` answer means this process's cursors are behind the
    /// broker's: re-seed before the next report.
    reseed_needed: bool,
}

impl Reporter {
    fn new(cfg: Config) -> Result<Self, String> {
        let broker = Broker::new(&cfg.broker_url, cfg.token_file.clone(), cfg.request_timeout)?;
        Ok(Self {
            cfg,
            broker,
            keys: KeyMinter::new(),
            org: String::new(),
            cursors: HashMap::new(),
            apps: HashMap::new(),
            outbox: VecDeque::new(),
            reseed_needed: false,
        })
    }

    async fn session(&mut self, stop: &mut watch::Receiver<bool>) -> Result<(), SessionEnd> {
        let me = self.broker.whoami().await?;
        if !me.capabilities.iter().any(|c| c == REPORT_DEPLOYMENTS) {
            return Err(SessionEnd::Failed(format!(
                "installation `{}` is not approved for {REPORT_DEPLOYMENTS}",
                me.name
            )));
        }
        if self.org != me.org {
            tracing::info!(
                installation = %me.name,
                org = %me.org,
                principal = %me.principal,
                "argocd: reporting as this installation"
            );
        }
        self.org = me.org;

        // Seed, then list: whatever the list shows is compared with what was
        // last ACCEPTED, so a restart reports only what changed meanwhile.
        self.reseed().await?;
        // Pending reports from a previous session go first, in order.
        self.flush(stop).await?;
        let list: ApplicationList = self.broker.argocd_get("/api/v1/applications", &[]).await?;
        let listed: BTreeSet<String> = list.items.iter().map(Application::target_id).collect();
        self.apps.retain(|target, _| listed.contains(target));
        for app in list.items {
            self.observe(app).await?;
        }
        self.flush(stop).await?;

        let resource_version = list.metadata.resource_version;
        let query: Vec<(&str, &str)> = if resource_version.is_empty() {
            vec![]
        } else {
            vec![("resourceVersion", resource_version.as_str())]
        };
        let resp = self
            .broker
            .argocd_stream("/api/v1/stream/applications", &query)
            .await?;
        let (tx, mut rx) = mpsc::channel(STREAM_CHANNEL);
        let reader = AbortOnDrop(tokio::spawn(read_ndjson(
            resp,
            tx,
            self.cfg.stream_idle_reconnect,
        )));
        tracing::info!(apps = self.apps.len(), "argocd: watching Applications");

        let mut pending: HashMap<String, (Application, Instant)> = HashMap::new();
        let mut resend_tick = tokio::time::interval(self.cfg.resend_poll);
        resend_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let next_due = pending.values().map(|(_, at)| *at).min();
            tokio::select! {
                _ = wait_stopped(stop) => return Err(SessionEnd::Stopped),
                msg = rx.recv() => match msg {
                    None => {
                        drop(reader);
                        return Err(SessionEnd::Reconnect("the watch stream ended".into()));
                    }
                    Some(StreamMsg::Line(line)) => {
                        if let Some(err) = line.error {
                            return Err(SessionEnd::Reconnect(format!(
                                "the watch stream reported an error: {}", err.message
                            )));
                        }
                        let Some(event) = line.result else { continue };
                        let target = event.application.target_id();
                        match event.kind.as_str() {
                            "DELETED" => {
                                // No status means "gone" (ADR-0070 §5); the
                                // board keeps what it last heard.
                                pending.remove(&target);
                                self.apps.remove(&target);
                            }
                            _ => {
                                let deadline = Instant::now() + self.cfg.debounce;
                                pending
                                    .entry(target)
                                    .and_modify(|(app, _)| *app = event.application.clone())
                                    .or_insert((event.application, deadline));
                            }
                        }
                    }
                },
                _ = sleep_until_opt(next_due), if next_due.is_some() => {
                    let now = Instant::now();
                    let mut ready: Vec<(String, Application, Instant)> = pending
                        .iter()
                        .filter(|(_, (_, at))| *at <= now)
                        .map(|(t, (app, at))| (t.clone(), app.clone(), *at))
                        .collect();
                    // Oldest change first: source order across Applications.
                    ready.sort_by_key(|(_, _, at)| *at);
                    for (target, app, _) in ready {
                        pending.remove(&target);
                        self.observe(app).await?;
                    }
                    self.flush(stop).await?;
                }
                _ = resend_tick.tick() => {
                    if self.reseed_needed {
                        self.reseed().await?;
                    }
                    self.resend_flagged().await?;
                    self.flush(stop).await?;
                }
            }
        }
    }

    /// Seed cursors from `/targets` (all pages): the last accepted sequence
    /// and state per target. A target with a report still queued here keeps
    /// what this process last decided, so the queued report is not decided
    /// again. Then re-assess every known Application against the result.
    async fn reseed(&mut self) -> Result<(), SessionEnd> {
        let targets = self.broker.targets(false).await?;
        let queued: BTreeSet<String> = self
            .outbox
            .iter()
            .map(|i| i.target_id().to_string())
            .collect();
        for t in targets {
            let cursor = self.cursors.entry(t.target_id.clone()).or_default();
            cursor.last_sequence = cursor.last_sequence.max(t.last_sequence);
            if queued.contains(&t.target_id) {
                continue;
            }
            cursor.vendor_grouping = Some(t.vendor_grouping);
            cursor.reported = match (t.last_version, t.last_status.as_deref(), t.last_label) {
                (Some(version), Some(status), Some(label)) => {
                    Status::parse(status).map(|status| Reported {
                        version,
                        status,
                        label,
                    })
                }
                _ => None,
            };
            cursor.owners = Some(t.owners);
        }
        self.reseed_needed = false;
        let known: Vec<String> = self.apps.keys().cloned().collect();
        for target in known {
            self.decide(&target, false);
        }
        Ok(())
    }

    /// Re-send every flagged target's current state (ADR-0070 §11) — the one
    /// exception to "transitions only". A target with no status here (not
    /// seen, or no version) is skipped, as is one with a report queued.
    async fn resend_flagged(&mut self) -> Result<(), SessionEnd> {
        let flagged = self.broker.targets(true).await?;
        for t in flagged.into_iter().filter(|t| t.resend) {
            if self.outbox.iter().any(|i| i.target_id() == t.target_id) {
                continue;
            }
            if self.apps.contains_key(&t.target_id) {
                tracing::info!(target_id = %t.target_id, "argocd: re-sending (flagged by core)");
                self.decide(&t.target_id, true);
            }
        }
        Ok(())
    }

    /// Take in an Application's latest state: re-read its managed resources
    /// if its manifests may have changed, then decide on a report.
    async fn observe(&mut self, app: Application) -> Result<(), SessionEnd> {
        if app.metadata.name.is_empty() || app.metadata.namespace.is_empty() {
            return Ok(());
        }
        let target = app.target_id();
        let fingerprint = app.manifests_fingerprint();
        let cached = self
            .apps
            .get(&target)
            .filter(|v| v.fingerprint == fingerprint)
            .and_then(|v| v.images.clone());
        let images = match cached {
            Some(images) => Some(images),
            None => self.read_images(&app).await?,
        };
        self.apps.insert(
            target.clone(),
            AppView {
                app,
                fingerprint,
                images,
            },
        );
        self.decide(&target, false);
        Ok(())
    }

    /// The Application's target pod-template images, from its managed
    /// resources. `None` when Argo has none to give (the Application went
    /// away meanwhile): it is tried again on its next change.
    async fn read_images(&self, app: &Application) -> Result<Option<BTreeSet<String>>, SessionEnd> {
        let path = format!(
            "/api/v1/applications/{}/managed-resources",
            encode_component(&app.metadata.name)
        );
        match self
            .broker
            .argocd_get::<ManagedResources>(
                &path,
                &[("appNamespace", app.metadata.namespace.as_str())],
            )
            .await
        {
            Ok(resources) => Ok(Some(target_images(&resources))),
            Err(e @ BrokerError::Status { status: 404, .. }) => {
                tracing::info!(target_id = %app.target_id(), error = %e, "argocd: no managed resources");
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Compare one Application with its cursor and queue what is due.
    fn decide(&mut self, target: &str, resend: bool) {
        let Some(view) = self.apps.get(target) else {
            return;
        };
        let Some(images) = view.images.as_ref() else {
            return;
        };
        let obs = match observe(
            &view.app,
            images,
            &self.org,
            &self.cfg.owner_annotation,
            self.cfg.argocd_ui_url.as_deref(),
        ) {
            Ok(obs) => obs,
            Err(Unreportable::NoImages) => {
                tracing::debug!(target_id = %target, "argocd: no pod-template images; not reported");
                return;
            }
            Err(why) => {
                tracing::warn!(target_id = %target, ?why, "argocd: cannot be reported");
                return;
            }
        };
        let cursor = self.cursors.get(target);
        match due(cursor, &obs, resend) {
            Due::Nothing => {}
            Due::Status => self.enqueue(&obs, true),
            Due::Ownership => self.enqueue(&obs, false),
        }
    }

    /// Queue a report for `obs` at the target's next sequence, and record it
    /// as reported right away — so the same state is not queued twice while
    /// it is in flight.
    fn enqueue(&mut self, obs: &Observation, status: bool) {
        let cursor = self.cursors.entry(obs.target_id.clone()).or_default();
        if cursor.last_sequence >= MAX_SEQUENCE {
            tracing::error!(
                target_id = %obs.target_id,
                "argocd: the target's sequence is exhausted; an admin must reset its cursor"
            );
            return;
        }
        let sequence = cursor.last_sequence + 1;
        cursor.last_sequence = sequence;
        cursor.owners = Some(obs.owners.clone());
        let item = if status {
            cursor.vendor_grouping = Some(obs.vendor_grouping.clone());
            cursor.reported = Some(Reported {
                version: obs.version.clone(),
                status: obs.assessment.status,
                label: obs.assessment.label.clone(),
            });
            status_item(obs, &self.keys, sequence, unix_ms())
        } else {
            ownership_item(obs, &self.keys, sequence)
        };
        tracing::info!(
            target_id = %obs.target_id,
            sequence,
            status = %obs.assessment.status.as_str(),
            label = %obs.assessment.label,
            kind = if status { "status" } else { "ownership" },
            "argocd: report queued"
        );
        self.outbox.push_back(item);
    }

    /// Send the queue, a batch of at most 100 at a time, reading each
    /// item's result (ADR-0070 §11). Returns when the queue is empty, or
    /// with an error that ends the session (the queue is kept for the next).
    async fn flush(&mut self, stop: &mut watch::Receiver<bool>) -> Result<(), SessionEnd> {
        while !self.outbox.is_empty() {
            let n = self.outbox.len().min(MAX_ITEMS_PER_BATCH);
            let batch: Vec<ReportItem> = self.outbox.iter().take(n).cloned().collect();
            match self.broker.post_reports(&batch).await {
                Ok(results) => {
                    for (item, result) in batch.iter().zip(results) {
                        self.read_result(item, &result.outcome, result.reason.as_deref());
                    }
                    self.outbox.drain(..n);
                }
                // The whole batch was rolled back: one key was reused for
                // other content — a reporter bug. Drop that item, resend the
                // rest unchanged.
                Err(BrokerError::Status {
                    status: 409, body, ..
                }) => {
                    let key = conflicting_key(&body).map(str::to_string);
                    match key
                        .and_then(|k| self.outbox.iter().position(|i| i.idempotency_key() == k))
                    {
                        Some(at) => {
                            let dropped = self.outbox.remove(at);
                            tracing::error!(?dropped, %body, "argocd: idempotency key reused; item dropped");
                        }
                        None => {
                            tracing::error!(%body, "argocd: 409 names no queued key; batch dropped");
                            self.outbox.drain(..n);
                        }
                    }
                    self.reseed_needed = true;
                }
                // The body itself was refused: nothing in it can succeed
                // unchanged.
                Err(BrokerError::Status {
                    status: 400, body, ..
                }) => {
                    tracing::error!(%body, items = n, "argocd: report batch refused (400); dropped");
                    self.outbox.drain(..n);
                    self.reseed_needed = true;
                }
                // Nothing was recorded: the same batch is safe to resend
                // after Retry-After.
                Err(BrokerError::Status {
                    status: status @ (429 | 503),
                    retry_after,
                    ..
                }) => {
                    let wait = retry_after.unwrap_or(if status == 429 {
                        DEFAULT_RETRY_AFTER_429
                    } else {
                        DEFAULT_RETRY_AFTER_503
                    });
                    tracing::info!(status, ?wait, "argocd: broker asked to back off");
                    if sleep_or_stop(wait, stop).await {
                        return Err(SessionEnd::Stopped);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn read_result(&mut self, item: &ReportItem, outcome: &str, reason: Option<&str>) {
        let target_id = item.target_id();
        let sequence = item.sequence();
        match outcome {
            "accepted" | "duplicate" => {
                tracing::debug!(%target_id, sequence, outcome, "argocd: report recorded");
            }
            "unmapped" => {
                // Recorded; no Environment lists the grouping yet. Core flags
                // the target `resend` once one does.
                tracing::info!(%target_id, sequence, "argocd: report unmapped (no Environment lists its project)");
            }
            "stale" => {
                // This process's cursor was behind core's: re-seed, then the
                // current state is decided again against what core has.
                tracing::warn!(%target_id, sequence, "argocd: report stale; re-seeding cursors");
                if let Some(c) = self.cursors.get_mut(target_id) {
                    c.reported = None;
                }
                self.reseed_needed = true;
            }
            "rejected" => {
                // Nothing stored, not even the key. Never resent unchanged:
                // the state stays marked as reported until it changes.
                tracing::error!(%target_id, sequence, reason = reason.unwrap_or(""), "argocd: report rejected");
            }
            other => {
                tracing::warn!(%target_id, sequence, outcome = other, "argocd: unknown report outcome");
            }
        }
    }

    /// On shutdown: one bounded attempt to deliver what is queued.
    async fn last_flush(&mut self) {
        if self.outbox.is_empty() {
            return;
        }
        let (_tx, mut never) = watch::channel(false);
        match tokio::time::timeout(self.cfg.shutdown_flush, self.flush(&mut never)).await {
            Ok(Ok(())) => tracing::info!("argocd: queued reports delivered before exit"),
            _ => tracing::warn!(
                queued = self.outbox.len(),
                "argocd: exiting with reports undelivered; the next start re-derives them"
            ),
        }
    }
}

/// Read NDJSON lines off the watch stream into `tx` until it ends, errors,
/// or stays silent for `idle`. Dropping `tx` (returning) tells the loop.
async fn read_ndjson(mut resp: reqwest::Response, tx: mpsc::Sender<StreamMsg>, idle: Duration) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = match tokio::time::timeout(idle, resp.chunk()).await {
            Err(_) => {
                tracing::info!(?idle, "argocd: watch stream idle; re-opening");
                return;
            }
            Ok(Err(e)) => {
                tracing::info!(error = %e, "argocd: watch stream broke");
                return;
            }
            Ok(Ok(None)) => return,
            Ok(Ok(Some(chunk))) => chunk,
        };
        buf.extend_from_slice(&chunk);
        while let Some(end) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=end).collect();
            let line = line.trim_ascii();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice::<StreamLine>(line) {
                Ok(parsed) => {
                    if tx.send(StreamMsg::Line(parsed)).await.is_err() {
                        return;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "argocd: unreadable watch line; skipped"),
            }
        }
        if buf.len() > MAX_STREAM_LINE_BYTES {
            tracing::warn!("argocd: watch line over the size limit; re-opening");
            return;
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn stopped(stop: &watch::Receiver<bool>) -> bool {
    *stop.borrow()
}

/// Resolves once `stop` is true (or its sender is gone).
async fn wait_stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|v| *v).await;
}

/// Sleep `d`; `true` if `stop` fired first.
async fn sleep_or_stop(d: Duration, stop: &mut watch::Receiver<bool>) -> bool {
    if stopped(stop) {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(d) => stopped(stop),
        _ = wait_stopped(stop) => true,
    }
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Exponential backoff with jitter (so many reporters behind one broker do
/// not reconnect in lockstep after a broker restart).
struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max,
            next: min,
        }
    }

    fn reset(&mut self) {
        self.next = self.min;
    }

    fn next(&mut self) -> Duration {
        let base = self.next;
        self.next = (self.next * 2).min(self.max);
        // Up to +25 %: a per-call random from a freshly seeded RandomState.
        let random = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        let jitter = base.mul_f64((random % 1000) as f64 / 4000.0);
        (base + jitter).min(self.max.max(self.min))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_its_cap_and_resets() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(8));
        let first = b.next();
        assert!(first >= Duration::from_secs(1) && first <= Duration::from_millis(1250));
        for _ in 0..10 {
            assert!(b.next() <= Duration::from_secs(8));
        }
        b.reset();
        assert!(b.next() <= Duration::from_millis(1250));
    }
}
