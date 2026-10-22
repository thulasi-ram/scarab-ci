//! The CredentialBroker's read proxy (ADR-0072 §5, MMP slice d3):
//! `GET|HEAD /_scarab/plugin/v1/connections/{slot}/{*path}`.
//!
//! For each request a Plugin sends, the broker
//!
//! 1. takes one of the installation's [`PROXY_CONCURRENCY_PER_INSTALLATION`]
//!    permits — in authentication, before any store or secret read, so a
//!    flood is turned away (429) without touching either;
//! 2. checks it against the manifest's **read rules**
//!    ([`scarab_plugin::match_read_request`]: decoded before matching, `*` =
//!    one segment, traversal and encoded separators refused, and only the
//!    query keys the matched rule lists, each once);
//! 3. reserves a slot in the journal queue — full, it refuses (503): it
//!    never forwards what it cannot journal;
//! 4. adds the credential — `Authorization: Bearer <secret>` — to an
//!    **allowlist** of the Plugin's headers ([`REQUEST_HEADERS`]); the
//!    Plugin's own `Authorization` (its ServiceAccount token) is not on it,
//!    so it never leaves the cluster;
//! 5. forwards the matched path and query, re-encoded canonically, to the
//!    PluginConnection's admin-bound `base_url`; and
//! 6. journals it, off the request path: a `head` row through the reserved
//!    slot when the response head arrives, an `end` row when the body ends.
//!
//! **The destination is always the admin-bound `base_url`.** The upstream
//! URL is built from the binding and the matched, re-encoded path and query
//! only — never from the request's authority, its `Host` header or anything
//! else a Plugin sends — so a Plugin cannot aim the credential at a host of
//! its choosing. That is the core property of brokered mode (ADR-0072 §6).
//!
//! The rest follows from the Plugin being an untrusted caller:
//!
//! - **headers are allowlisted both ways**: a Plugin cannot steer the
//!   upstream's method, path or identity with a header the upstream honours
//!   (`X-HTTP-Method-Override`, `X-Original-URL`, grpc-gateway's
//!   `Grpc-Metadata-Token`, Kubernetes' `Impersonate-User`), and gets back
//!   only [`RESPONSE_HEADERS`] — never a session or anything carrying the
//!   credential;
//! - **no request body** (a read carries none) and **no redirects** — a 3xx
//!   is handed back, never followed, so an upstream cannot bounce the
//!   credential elsewhere;
//! - a custom CA is the **only** trust anchor for its connection, and there
//!   is no skip-verify;
//! - responses **stream**, and every stream re-checks the kill switch (and
//!   its binding) every [`KILL_RECHECK_INTERVAL`]: a kill, an uninstall, a
//!   re-bind or an unbind cuts a stream already open within that interval,
//!   not when it happens to end — and a cut stream always ends in an error,
//!   never a clean end the Plugin could take for the whole response.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use scarab_plugin::{
    match_read_request, rule_label, BrokerJournalEntry, BrokerJournalPhase, ConnectionSlot,
    InstallationState, PluginCapability, PluginConnection, PluginStore, ReadRequestMatch,
    ReadRequestRefusal,
};

use super::AppState;
use crate::broker::{PluginCaller, BROKER_PREFIX};
use crate::plugins::{plugin_connection_secret_scope, PLUGIN_CONNECTION_CREDENTIALS_ORG};

/// How often an open response stream re-reads the kill switch and its
/// binding. A kill reaches an open stream within this long.
pub const KILL_RECHECK_INTERVAL: Duration = Duration::from_secs(5);
/// How long one re-check may take. A re-check that times out or fails to
/// read counts as unreadable; [`KILL_RECHECK_UNREADABLE_TO_CUT`] in a row
/// cut the stream. A read that *says* killed or re-bound cuts at once.
pub const KILL_RECHECK_TIMEOUT: Duration = Duration::from_secs(2);
pub const KILL_RECHECK_UNREADABLE_TO_CUT: u32 = 2;
/// Proxied requests one installation may have in flight in this process. A
/// reporter needs one long watch stream plus a few short reads.
pub const PROXY_CONCURRENCY_PER_INSTALLATION: usize = 16;
/// The journal queue between requests and the writer (the default of
/// [`AppState::broker_journal_queue`]).
pub const JOURNAL_QUEUE: usize = 4096;
/// Upstream connect timeout, and how long the upstream has to send its
/// response head. There is no total timeout: a watch stream is meant to stay
/// open, and the kill re-check is what bounds it.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UPSTREAM_HEADERS_TIMEOUT: Duration = Duration::from_secs(30);
/// How many entries the writer folds into one INSERT.
const JOURNAL_BATCH: usize = 256;
/// How much of a request's path, query and slot the journal keeps.
const MAX_JOURNAL_PATH_CHARS: usize = 2048;
const MAX_JOURNAL_QUERY_CHARS: usize = 2048;
const MAX_JOURNAL_SLOT_CHARS: usize = 63;
const RETRY_AFTER_TOO_MANY_SECS: u64 = 1;
const RETRY_AFTER_BUSY_SECS: u64 = 5;
/// Response chunks buffered between the upstream and the Plugin.
const STREAM_BUFFER_CHUNKS: usize = 16;

/// The Plugin's headers that may travel upstream — content negotiation,
/// conditional reads and an identifying agent, nothing that could change
/// what the upstream does or as whom. (No `Range` or `Cache-Control`: the
/// Argo CD reporter needs neither.)
pub const REQUEST_HEADERS: [&str; 6] = [
    "accept",
    "accept-encoding",
    "accept-language",
    "if-none-match",
    "if-modified-since",
    "user-agent",
];

/// The upstream's headers that may travel back: what describes the body and
/// its caching, and `retry-after`. Not `set-cookie` or any other session, not
/// `location` or `www-authenticate` (they can name the bound URL and realm).
pub const RESPONSE_HEADERS: [&str; 10] = [
    "content-type",
    "content-length",
    "content-encoding",
    "etag",
    "last-modified",
    "cache-control",
    "date",
    "vary",
    "retry-after",
    "expires",
];

/// The route's path, as `broker::router` registers it.
pub fn route_path() -> String {
    format!("{BROKER_PREFIX}/connections/{{slot}}/{{*path}}")
}

/// Marks a response as the upstream's own: the broker's 500-scrubber
/// (`broker::scrub_internal_errors`) passes it through, since its body is the
/// vendor's, not an internal error of Scarab's.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UpstreamResponse;

/// One of an installation's [`PROXY_CONCURRENCY_PER_INSTALLATION`] permits,
/// taken in authentication and held until the response stream ends.
#[derive(Clone)]
pub(crate) struct ProxyPermit(#[allow(dead_code)] Arc<OwnedSemaphorePermit>);

/// What the proxy route shares across requests: the registry on the
/// broker's pool, the journal queue, the per-installation permits and the
/// upstream clients (one per distinct CA).
pub struct BrokerProxy {
    store: Arc<dyn PluginStore>,
    journal: Option<mpsc::Sender<BrokerJournalEntry>>,
    /// Refusals not journaled because the queue was over half full.
    refusals_unjournaled: AtomicU64,
    permits: Mutex<HashMap<String, Arc<Semaphore>>>,
    clients: Mutex<HashMap<String, reqwest::Client>>,
}

impl BrokerProxy {
    /// A proxy over `store`, with a `journal_queue`-entry journal queue and
    /// its writer spawned on the current Tokio runtime. Without a runtime
    /// there is no writer, and the proxy refuses every request (503): it
    /// never forwards what it cannot journal.
    pub fn spawn(store: Arc<dyn PluginStore>, journal_queue: usize) -> Arc<Self> {
        let journal = tokio::runtime::Handle::try_current().ok().map(|rt| {
            let (tx, rx) = mpsc::channel(journal_queue.max(2));
            rt.spawn(write_journal(store.clone(), rx));
            tx
        });
        if journal.is_none() {
            tracing::error!(
                "broker: no Tokio runtime for the journal writer; the read proxy is off"
            );
        }
        Arc::new(Self {
            store,
            journal,
            refusals_unjournaled: AtomicU64::new(0),
            permits: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
        })
    }

    fn try_permit(&self, installation_id: &str) -> Option<OwnedSemaphorePermit> {
        let semaphore = {
            let mut map = self
                .permits
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            map.entry(installation_id.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(PROXY_CONCURRENCY_PER_INSTALLATION)))
                .clone()
        };
        semaphore.try_acquire_owned().ok()
    }

    /// A slot in the journal queue, held until the request's row is sent
    /// through it — so that row cannot be dropped. `None` = full.
    fn reserve_journal(&self) -> Option<mpsc::OwnedPermit<BrokerJournalEntry>> {
        self.journal.clone()?.try_reserve_owned().ok()
    }

    /// Journal a refusal — while the queue is under half full. Past that,
    /// refusals are counted and logged instead, so a flood of them can never
    /// take the room a forwarded request needs to be journaled.
    fn journal_refusal(&self, entry: BrokerJournalEntry) {
        let Some(tx) = &self.journal else {
            return;
        };
        if tx.capacity() > tx.max_capacity() / 2 && tx.try_send(entry).is_ok() {
            return;
        }
        let unjournaled = self.refusals_unjournaled.fetch_add(1, Ordering::Relaxed) + 1;
        // Logged at 1, 2, 4, 8, …: a flood is visible without flooding the log.
        if unjournaled.is_power_of_two() {
            tracing::warn!(
                unjournaled,
                "broker: journal queue over half full; refusals are counted, not journaled"
            );
        }
    }

    /// Journal a stream's `end` row; never waits. Its `head` row went
    /// through a reserved slot, so a full queue costs only the byte count
    /// and how it ended.
    fn journal_end(&self, entry: BrokerJournalEntry) {
        let Some(tx) = &self.journal else {
            return;
        };
        if let Err(e) = tx.try_send(entry) {
            tracing::error!(error = %e, "broker: journal queue full; an end row was dropped");
        }
    }

    /// The upstream client for a connection's CA (one per distinct CA).
    fn client(&self, ca_pem: Option<&str>) -> Result<reqwest::Client, String> {
        let key = ca_pem.map_or_else(
            || "-".to_string(),
            |pem| {
                Sha256::digest(pem.as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect()
            },
        );
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let client = upstream_client(ca_pem)?;
        clients.insert(key, client.clone());
        Ok(client)
    }

    /// Is the stream still allowed to run: the installation enabled, and
    /// the connection still bound to the credential it started with?
    async fn still_authorised(
        &self,
        installation_id: &str,
        slot: &str,
        credential_ref: &str,
    ) -> Result<(), Recheck> {
        match self.store.installation_by_id(installation_id).await {
            Ok(Some(i)) if i.state == InstallationState::Enabled => {}
            Ok(_) => {
                return Err(Recheck::Decided(
                    "cut: the installation was killed or uninstalled",
                ))
            }
            Err(_) => {
                return Err(Recheck::Unreadable(
                    "cut: the kill switch could not be read",
                ))
            }
        }
        match self.store.connection(installation_id, slot).await {
            Ok(Some(c)) if c.credential_ref == credential_ref => Ok(()),
            Ok(_) => Err(Recheck::Decided(
                "cut: the connection was re-bound or unbound",
            )),
            Err(_) => Err(Recheck::Unreadable("cut: the connection could not be read")),
        }
    }
}

/// Why a stream's re-check did not pass.
enum Recheck {
    /// The store says stop: killed, uninstalled, re-bound or unbound.
    Decided(&'static str),
    /// The store could not say. Not proof the stream may go on, nor that it
    /// must stop: [`KILL_RECHECK_UNREADABLE_TO_CUT`] in a row cut it.
    Unreadable(&'static str),
}

/// Take one of the installation's proxy permits, before anything reads the
/// store or a secret (`broker::authenticate`). Over the limit, the request
/// (`method uri_path`) is refused (429) — journaled like any refusal.
pub(crate) async fn take_permit(
    st: &AppState,
    proxy: &BrokerProxy,
    installation_id: &str,
    method: &str,
    uri_path: &str,
) -> Result<ProxyPermit, Response> {
    if let Some(permit) = proxy.try_permit(installation_id) {
        return Ok(ProxyPermit(Arc::new(permit)));
    }
    let started = Instant::now();
    let (slot, path) = split_connection_path(uri_path).unwrap_or(("", String::new()));
    let draft = Draft::new(
        installation_id,
        slot,
        method,
        &path,
        st.clock.now().await.0,
        started,
    );
    let refusal = Refusal::new(
        StatusCode::TOO_MANY_REQUESTS,
        "concurrency",
        format!(
            "at most {PROXY_CONCURRENCY_PER_INSTALLATION} broker requests per installation at \
             once"
        ),
    )
    .retry_after(RETRY_AFTER_TOO_MANY_SECS);
    proxy.journal_refusal(draft.refused(refusal.status, refusal.reason));
    Err(refusal.respond())
}

/// An upstream client for one connection: no redirects (a 3xx goes back to
/// the Plugin, so the credential is never sent where the admin did not bind
/// it), no environment proxy (the destination is exactly the bound one), and
/// — with a CA — that CA as the ONLY trust anchor. There is no skip-verify.
/// Also what the admin API uses to check a CA before binding it.
pub(crate) fn upstream_client(ca_pem: Option<&str>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT);
    if let Some(pem) = ca_pem {
        let certs = reqwest::Certificate::from_pem_bundle(pem.as_bytes())
            .map_err(|e| format!("the CA is not PEM certificates: {e}"))?;
        if certs.is_empty() {
            return Err("the CA holds no PEM certificate".into());
        }
        builder = builder.tls_certs_only(certs);
    }
    builder
        .build()
        .map_err(|e| format!("cannot build a TLS client with this CA: {e}"))
}

/// Drain the journal queue into the store, a batch per round trip. A
/// failed write is retried once; then the batch is logged as lost.
async fn write_journal(store: Arc<dyn PluginStore>, mut rx: mpsc::Receiver<BrokerJournalEntry>) {
    let mut batch = Vec::with_capacity(JOURNAL_BATCH);
    while let Some(first) = rx.recv().await {
        batch.push(first);
        while batch.len() < JOURNAL_BATCH {
            match rx.try_recv() {
                Ok(entry) => batch.push(entry),
                Err(_) => break,
            }
        }
        if let Err(first) = store.append_broker_journal(&batch).await {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Err(again) = store.append_broker_journal(&batch).await {
                tracing::error!(
                    lost = batch.len(),
                    first = %first,
                    error = %again,
                    "broker: journal write failed twice; entries lost"
                );
            }
        }
        batch.clear();
    }
}

/// One request's journal rows in the making.
struct Draft {
    request_id: String,
    installation_id: String,
    slot: String,
    method: String,
    path: String,
    upstream_path: Option<String>,
    query: Option<String>,
    rule: Option<String>,
    started_at: i64,
    started: Instant,
}

impl Draft {
    fn new(
        installation_id: &str,
        slot: &str,
        method: &str,
        raw_path: &str,
        started_at: i64,
        started: Instant,
    ) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            installation_id: installation_id.to_string(),
            slot: bounded(slot, MAX_JOURNAL_SLOT_CHARS),
            method: bounded(method, 16),
            path: bounded(raw_path, MAX_JOURNAL_PATH_CHARS),
            upstream_path: None,
            query: None,
            rule: None,
            started_at,
            started,
        }
    }

    fn row(
        &self,
        phase: BrokerJournalPhase,
        status: StatusCode,
        bytes: Option<u64>,
        denied_reason: Option<String>,
    ) -> BrokerJournalEntry {
        BrokerJournalEntry {
            request_id: self.request_id.clone(),
            phase,
            installation_id: self.installation_id.clone(),
            slot: self.slot.clone(),
            method: self.method.clone(),
            path: self.path.clone(),
            upstream_path: self.upstream_path.clone(),
            query: self.query.clone(),
            rule: self.rule.clone(),
            status: Some(status.as_u16()),
            bytes,
            started_at: self.started_at,
            duration_ms: Some(self.started.elapsed().as_millis().min(u64::MAX as u128) as u64),
            denied_reason,
        }
    }

    fn refused(&self, status: StatusCode, reason: &str) -> BrokerJournalEntry {
        self.row(
            BrokerJournalPhase::Refused,
            status,
            None,
            Some(reason.to_string()),
        )
    }

    fn head(&self, status: StatusCode) -> BrokerJournalEntry {
        self.row(BrokerJournalPhase::Head, status, None, None)
    }

    fn end(&self, status: StatusCode, bytes: u64, why: Option<&str>) -> BrokerJournalEntry {
        self.row(
            BrokerJournalPhase::End,
            status,
            Some(bytes),
            why.map(str::to_string),
        )
    }
}

/// Bounded journal text: at most `max` characters, no NUL (Postgres TEXT
/// refuses it).
fn bounded(s: &str, max: usize) -> String {
    s.chars().filter(|c| *c != '\0').take(max).collect()
}

/// Why the broker refused a request: journaled (as `reason`), then answered.
struct Refusal {
    status: StatusCode,
    reason: &'static str,
    message: String,
    retry_after: Option<u64>,
}

impl Refusal {
    fn new(status: StatusCode, reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            message: message.into(),
            retry_after: None,
        }
    }

    fn retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs);
        self
    }

    fn respond(self) -> Response {
        let mut resp = (self.status, self.message).into_response();
        if let Some(secs) = self.retry_after {
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        resp
    }
}

/// `…/connections/{slot}/{rest}` → (`slot`, `/{rest}`), both still exactly
/// as sent (percent-encoded). Axum's `Path` would decode them, and the
/// matcher must see the raw text to refuse `%2F` and friends.
fn split_connection_path(path: &str) -> Option<(&str, String)> {
    let rest = path.strip_prefix(&format!("{BROKER_PREFIX}/connections/"))?;
    let (slot, tail) = rest.split_once('/')?;
    Some((slot, format!("/{tail}")))
}

/// The header names a `Connection` header marks hop-by-hop: dropped even if
/// allowlisted, since they describe one hop, never the message.
fn connection_listed(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// The Plugin's headers that travel upstream: [`REQUEST_HEADERS`] only.
/// Everything else — its `Authorization` (the bound credential replaces
/// it), `Cookie`, `Host`, forwarding metadata, and any header an upstream
/// might honour to change the method, path or identity — stays behind.
fn forwardable_request_headers(headers: &HeaderMap) -> HeaderMap {
    let listed = connection_listed(headers);
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        if REQUEST_HEADERS.contains(&n) && !listed.iter().any(|l| l == n) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// The upstream's headers that travel back: [`RESPONSE_HEADERS`] only, and
/// none whose value holds the credential (an upstream that echoes it).
fn forwardable_response_headers(headers: &HeaderMap, credential: &[u8]) -> HeaderMap {
    let listed = connection_listed(headers);
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        if !RESPONSE_HEADERS.contains(&n) || listed.iter().any(|l| l == n) {
            continue;
        }
        if contains(value.as_bytes(), credential) {
            tracing::warn!(
                header = n,
                "broker: an upstream header echoed the credential; dropped"
            );
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Does the request carry (or announce) a body?
fn has_body(headers: &HeaderMap) -> bool {
    headers.contains_key(header::TRANSFER_ENCODING)
        || headers
            .get(header::CONTENT_LENGTH)
            .is_some_and(|v| v.to_str().map_or(true, |s| s.trim() != "0"))
}

/// The upstream URL: the bound base, then the matched path and query, both
/// already re-encoded canonically by the matcher. Checked to still be the
/// bound origin under the bound path — the matcher's canonical encoding
/// already guarantees it; this makes the core property hold even if that
/// ever regressed.
fn upstream_url(
    connection: &PluginConnection,
    upstream_path: &str,
    query: Option<&str>,
) -> Result<reqwest::Url, String> {
    let base = reqwest::Url::parse(&connection.base_url)
        .map_err(|e| format!("bound base URL unreadable: {e}"))?;
    let mut url = reqwest::Url::parse(&format!("{}{upstream_path}", connection.base_url))
        .map_err(|e| format!("upstream URL unreadable: {e}"))?;
    let prefix = base.path().trim_end_matches('/');
    if url.origin() != base.origin() || !url.path().starts_with(&format!("{prefix}/")) {
        return Err(format!(
            "upstream URL {url} left the bound destination {}",
            connection.base_url
        ));
    }
    url.set_query(query);
    Ok(url)
}

/// `GET|HEAD /_scarab/plugin/v1/connections/{slot}/{*path}` — authenticated,
/// kill-switch-checked and holding one of the installation's permits
/// (`broker::authenticate`) before it runs.
pub(crate) async fn proxy(
    State(st): State<AppState>,
    Extension(proxy): Extension<Arc<BrokerProxy>>,
    Extension(PluginCaller(installation)): Extension<PluginCaller>,
    Extension(permit): Extension<ProxyPermit>,
    req: Request,
) -> Response {
    let started = Instant::now();
    let started_at = st.clock.now().await.0;
    let Some((slot_raw, raw_path)) = split_connection_path(req.uri().path()) else {
        return (StatusCode::NOT_FOUND, "no such broker route").into_response();
    };
    let mut draft = Draft::new(
        &installation.id,
        slot_raw,
        req.method().as_str(),
        &raw_path,
        started_at,
        started,
    );
    // Until a journal slot is reserved, a refusal is journaled while the
    // queue is under half full, and counted past that.
    let refuse = |draft: &Draft, refusal: Refusal| {
        proxy.journal_refusal(draft.refused(refusal.status, refusal.reason));
        refusal.respond()
    };

    if !installation.manifest.has(PluginCapability::UseConnection) {
        return refuse(
            &draft,
            Refusal::new(
                StatusCode::FORBIDDEN,
                "capability",
                format!(
                    "plugin installation `{}` was not approved for use_connection",
                    installation.name
                ),
            ),
        );
    }
    let Some(slot) = installation
        .manifest
        .connections
        .iter()
        .find(|s| s.name == slot_raw)
    else {
        let message = format!(
            "plugin installation `{}` declares no connection slot `{}`",
            installation.name, draft.slot
        );
        return refuse(
            &draft,
            Refusal::new(StatusCode::NOT_FOUND, "unknown slot", message),
        );
    };
    if has_body(req.headers()) {
        return refuse(
            &draft,
            Refusal::new(
                StatusCode::BAD_REQUEST,
                "request body",
                "a read carries no request body; the broker forwards none (ADR-0072 §5)",
            ),
        );
    }
    let matched =
        match match_read_request(slot, req.method().as_str(), &raw_path, req.uri().query()) {
            Ok(m) => m,
            Err(refusal @ ReadRequestRefusal::MethodNotAllowed(_)) => {
                let mut resp = refuse(
                    &draft,
                    Refusal::new(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "method",
                        format!("{refusal}; write rules are not available (ADR-0072 MMP)"),
                    ),
                );
                resp.headers_mut()
                    .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
                return resp;
            }
            Err(refusal @ ReadRequestRefusal::QueryRefused(_)) => {
                // The query as sent: what was refused is the audit's business.
                draft.query = req
                    .uri()
                    .query()
                    .map(|q| bounded(q, MAX_JOURNAL_QUERY_CHARS));
                return refuse(
                    &draft,
                    Refusal::new(StatusCode::FORBIDDEN, "query", refusal.to_string()),
                );
            }
            Err(refusal) => {
                return refuse(
                    &draft,
                    Refusal::new(StatusCode::FORBIDDEN, "no read rule", refusal.to_string()),
                )
            }
        };
    draft.rule = Some(rule_label(matched.rule));
    draft.upstream_path = Some(bounded(&matched.upstream_path, MAX_JOURNAL_PATH_CHARS));
    draft.query = matched
        .upstream_query
        .as_deref()
        .map(|q| bounded(q, MAX_JOURNAL_QUERY_CHARS));

    // Never forward what cannot be journaled: from here on, every outcome
    // is journaled through this slot.
    let Some(journal_slot) = proxy.reserve_journal() else {
        tracing::error!(installation = %installation.id, slot = %slot.name,
            "broker: journal queue full; refusing a read it could not journal");
        return Refusal::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "journal full",
            "the broker is busy; retry",
        )
        .retry_after(RETRY_AFTER_BUSY_SECS)
        .respond();
    };
    let headers = forwardable_request_headers(req.headers());
    let method = req.method().clone();
    // The request body (none, checked above) is dropped here, never read.
    drop(req);

    let (connection, credential, upstream) = match send_upstream(
        &st,
        &proxy,
        &installation.id,
        slot,
        &matched,
        method,
        headers,
    )
    .await
    {
        Ok(sent) => sent,
        Err(refusal) => {
            journal_slot.send(draft.refused(refusal.status, refusal.reason));
            return refusal.respond();
        }
    };

    let status = upstream.status();
    let response_headers = forwardable_response_headers(upstream.headers(), credential.as_bytes());
    drop(credential);
    journal_slot.send(draft.head(status));

    let cut = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(STREAM_BUFFER_CHUNKS);
    let pump = Pump {
        proxy: proxy.clone(),
        installation_id: installation.id.clone(),
        slot: slot.name.clone(),
        credential_ref: connection.credential_ref.clone(),
        cut: cut.clone(),
        unreadable: AtomicU32::new(0),
    };
    tokio::spawn(async move {
        let _permit = permit;
        let (bytes, end) = pump.run(upstream, tx).await;
        pump.proxy.journal_end(draft.end(status, bytes, end));
    });

    let mut resp = Response::new(Body::from_stream(body_stream(rx, cut)));
    *resp.status_mut() = status;
    *resp.headers_mut() = response_headers;
    resp.extensions_mut().insert(UpstreamResponse);
    resp
}

/// What the Plugin reads: the pumped chunks, then — if the stream was cut
/// or failed — an error, whatever the buffer held. The pump sets `cut`
/// before it drops its sender, so the flag is set by the time the chunks
/// run out: a Plugin that stopped reading and drains the buffer later still
/// ends on an error, never on a clean end it could take for the whole body.
fn body_stream(
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
    cut: Arc<AtomicBool>,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    let tail = futures::stream::once(async move {
        cut.load(Ordering::SeqCst).then(|| {
            Err(std::io::Error::other(
                "the broker cut this response short (ADR-0072 §10)",
            ))
        })
    })
    .filter_map(std::future::ready);
    tokio_stream::wrappers::ReceiverStream::new(rx).chain(tail)
}

/// The bound connection, its credential, and the upstream's response head —
/// or why not.
async fn send_upstream(
    st: &AppState,
    proxy: &BrokerProxy,
    installation_id: &str,
    slot: &ConnectionSlot,
    matched: &ReadRequestMatch<'_>,
    method: axum::http::Method,
    mut headers: HeaderMap,
) -> Result<(PluginConnection, String, reqwest::Response), Refusal> {
    let (connection, credential) = bound_credential(st, proxy, installation_id, slot).await?;
    // Validated as visible ASCII at bind time; refuse rather than send a
    // malformed header if the store holds anything else.
    let credential = String::from_utf8(credential).map_err(|_| unusable_credential())?;
    let mut bearer = HeaderValue::from_str(&format!("Bearer {credential}"))
        .map_err(|_| unusable_credential())?;
    bearer.set_sensitive(true);

    let url = upstream_url(
        &connection,
        &matched.upstream_path,
        matched.upstream_query.as_deref(),
    )
    .map_err(|e| {
        Refusal::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "destination",
            format!("broker: {e}"),
        )
    })?;
    let client = proxy.client(connection.ca_pem.as_deref()).map_err(|e| {
        Refusal::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "client",
            format!(
                "broker: upstream client for connection `{}`: {e}",
                slot.name
            ),
        )
    })?;
    headers.insert(header::AUTHORIZATION, bearer);

    match tokio::time::timeout(
        UPSTREAM_HEADERS_TIMEOUT,
        client.request(method, url).headers(headers).send(),
    )
    .await
    {
        Ok(Ok(resp)) => Ok((connection, credential, resp)),
        Ok(Err(e)) => {
            // The error text names the bound URL: an operator's business,
            // not the Plugin's.
            tracing::warn!(installation = %installation_id, slot = %slot.name, error = %e,
                "broker: upstream request failed");
            Err(Refusal::new(
                StatusCode::BAD_GATEWAY,
                "upstream unreachable",
                format!(
                    "connection `{}`: the upstream could not be reached",
                    slot.name
                ),
            ))
        }
        Err(_) => Err(Refusal::new(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream timeout",
            format!(
                "connection `{}`: the upstream sent no response within {}s",
                slot.name,
                UPSTREAM_HEADERS_TIMEOUT.as_secs()
            ),
        )),
    }
}

fn unusable_credential() -> Refusal {
    Refusal::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "credential unusable",
        "broker: a bound credential is not a valid header value",
    )
}

/// The slot's binding and its credential's bytes. A re-bind between the
/// two reads deletes the secret the first read names; the binding is then
/// read once more, so a valid re-bind is not mistaken for a missing
/// credential.
async fn bound_credential(
    st: &AppState,
    proxy: &BrokerProxy,
    installation_id: &str,
    slot: &ConnectionSlot,
) -> Result<(PluginConnection, Vec<u8>), Refusal> {
    let mut reread = false;
    loop {
        let connection = match proxy.store.connection(installation_id, &slot.name).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                return Err(Refusal::new(
                    StatusCode::CONFLICT,
                    "not bound",
                    format!(
                        "connection `{}` is not bound: an org admin has not set its destination \
                         and credential yet",
                        slot.name
                    ),
                ))
            }
            Err(e) => {
                tracing::warn!(error = %e, "broker: connection read failed");
                return Err(Refusal::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "store busy",
                    "the broker is busy; retry",
                )
                .retry_after(RETRY_AFTER_BUSY_SECS));
            }
        };
        let Some(secrets) = st.secrets.as_deref() else {
            // A misconfiguration, not the Plugin's business: scrubbed to a
            // request id on the way out.
            return Err(Refusal::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "no secret store",
                "broker: no secret store wired",
            ));
        };
        match secrets
            .get(
                &plugin_connection_secret_scope(),
                &connection.credential_ref,
            )
            .await
        {
            Ok(secret) => return Ok((connection, secret.value)),
            Err(scarab_secrets::SecretError::NotFound) if !reread => reread = true,
            Err(scarab_secrets::SecretError::NotFound) => {
                tracing::error!(
                    installation = %installation_id,
                    slot = %slot.name,
                    scope = PLUGIN_CONNECTION_CREDENTIALS_ORG,
                    "broker: a bound connection's credential is missing from the secret store"
                );
                return Err(Refusal::new(
                    StatusCode::CONFLICT,
                    "credential missing",
                    format!(
                        "connection `{}` has no credential: an org admin must bind it again",
                        slot.name
                    ),
                ));
            }
            Err(e) => {
                return Err(Refusal::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "secret store",
                    format!("broker: secret store: {e}"),
                ))
            }
        }
    }
}

/// Moves one upstream response body to the Plugin, re-checking the kill
/// switch and the binding while it runs.
struct Pump {
    proxy: Arc<BrokerProxy>,
    installation_id: String,
    slot: String,
    credential_ref: String,
    /// Set before the sender drops when the stream did not simply end: the
    /// Plugin's body then ends in an error ([`body_stream`]).
    cut: Arc<AtomicBool>,
    /// Unreadable re-checks in a row.
    unreadable: AtomicU32,
}

impl Pump {
    /// Returns the bytes delivered and, if the stream did not simply end,
    /// why (`cut: …` for a kill-switch or re-bind cut).
    async fn run(
        &self,
        upstream: reqwest::Response,
        tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    ) -> (u64, Option<&'static str>) {
        let mut body = upstream.bytes_stream();
        let mut recheck = tokio::time::interval_at(
            tokio::time::Instant::now() + KILL_RECHECK_INTERVAL,
            KILL_RECHECK_INTERVAL,
        );
        recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut bytes = 0u64;
        let end = loop {
            tokio::select! {
                _ = recheck.tick() => {
                    if let Err(why) = self.check().await {
                        break Some(self.cut(&tx, why).await);
                    }
                }
                _ = tx.closed() => break None,
                next = body.next() => match next {
                    None => break None,
                    Some(Err(e)) => {
                        tracing::warn!(installation = %self.installation_id, slot = %self.slot,
                            error = %e, "broker: upstream body failed mid-stream");
                        self.cut.store(true, Ordering::SeqCst);
                        send_error(&tx, "the upstream response failed").await;
                        break Some("upstream body error");
                    }
                    Some(Ok(chunk)) => {
                        let len = chunk.len() as u64;
                        match self.deliver(&tx, chunk, &mut recheck).await {
                            Ok(()) => bytes += len,
                            Err(Some(why)) => break Some(why),
                            // The Plugin went away.
                            Err(None) => break None,
                        }
                    }
                },
            }
        };
        (bytes, end)
    }

    /// Hand one chunk to the Plugin. A Plugin that stops reading does not
    /// stop the re-check: while it waits for room, the kill switch is still
    /// read every interval. `Err(None)` = the Plugin went away.
    async fn deliver(
        &self,
        tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
        chunk: Bytes,
        recheck: &mut tokio::time::Interval,
    ) -> Result<(), Option<&'static str>> {
        let mut chunk = Some(chunk);
        loop {
            tokio::select! {
                room = tx.reserve() => {
                    let room = room.map_err(|_| None)?;
                    room.send(Ok(chunk.take().expect("sent once")));
                    return Ok(());
                }
                _ = recheck.tick() => {
                    if let Err(why) = self.check().await {
                        return Err(Some(self.cut(tx, why).await));
                    }
                }
            }
        }
    }

    /// One re-check, bounded by [`KILL_RECHECK_TIMEOUT`]. `Err` = cut now:
    /// the store said stop, or could not say [`KILL_RECHECK_UNREADABLE_TO_CUT`]
    /// times in a row — a kill switch that cannot be read is not proof it is
    /// off, but one slow read is not proof it is on.
    async fn check(&self) -> Result<(), &'static str> {
        let unreadable = match tokio::time::timeout(
            KILL_RECHECK_TIMEOUT,
            self.proxy
                .still_authorised(&self.installation_id, &self.slot, &self.credential_ref),
        )
        .await
        {
            Ok(Ok(())) => {
                self.unreadable.store(0, Ordering::Relaxed);
                return Ok(());
            }
            Ok(Err(Recheck::Decided(why))) => return Err(why),
            Ok(Err(Recheck::Unreadable(why))) => why,
            Err(_) => "cut: the kill switch re-check timed out",
        };
        let in_a_row = self.unreadable.fetch_add(1, Ordering::Relaxed) + 1;
        if in_a_row >= KILL_RECHECK_UNREADABLE_TO_CUT {
            return Err(unreadable);
        }
        tracing::warn!(installation = %self.installation_id, slot = %self.slot, why = unreadable,
            "broker: a stream re-check could not be read; cut if the next one cannot either");
        Ok(())
    }

    /// End the Plugin's response with an error — an aborted body, never a
    /// clean end it could mistake for the whole response.
    async fn cut(
        &self,
        tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
        why: &'static str,
    ) -> &'static str {
        tracing::warn!(installation = %self.installation_id, slot = %self.slot, why,
            "broker: response stream cut (ADR-0072 §10)");
        self.cut.store(true, Ordering::SeqCst);
        send_error(tx, why).await;
        why
    }
}

/// Tell a Plugin that is reading why its body ends. Waits briefly for room;
/// a Plugin that stopped reading may not get this message, but its body still
/// ends in an error once it drains the buffer ([`body_stream`]).
async fn send_error(tx: &mpsc::Sender<Result<Bytes, std::io::Error>>, why: &str) {
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        tx.send(Err(std::io::Error::other(why.to_string()))),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderName;
    use scarab_plugin::PluginConnectionAuthScheme;

    fn bound(base_url: &str) -> PluginConnection {
        PluginConnection {
            installation_id: "0000000001".into(),
            slot: "argocd".into(),
            base_url: base_url.into(),
            auth_scheme: PluginConnectionAuthScheme::Bearer,
            credential_ref: "pc-x".into(),
            ca_pem: None,
            updated_by: "amy".into(),
            updated_at: 1,
        }
    }

    #[test]
    fn the_upstream_url_is_the_bound_base_plus_the_matched_path() {
        let url = upstream_url(
            &bound("https://argo.acme.test/argo"),
            "/api/v1/applications",
            Some("projects=prod&selector=a%3Db"),
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://argo.acme.test/argo/api/v1/applications?projects=prod&selector=a%3Db"
        );
        let url = upstream_url(&bound("http://127.0.0.1:9"), "/api/version", None).unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:9/api/version");
    }

    #[test]
    fn a_path_that_would_leave_the_bound_destination_is_refused() {
        // The matcher never produces these; the check holds even if it did.
        for path in ["/../../etc", "@evil.test/x", "/%2e%2e/x"] {
            assert!(
                upstream_url(&bound("https://argo.acme.test/argo"), path, None).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn only_allowlisted_request_headers_go_upstream() {
        let mut h = HeaderMap::new();
        for (name, value) in [
            ("authorization", "Bearer sa-token"),
            ("host", "evil.test"),
            ("cookie", "session=1"),
            ("connection", "keep-alive, accept-language"),
            ("accept-language", "en"),
            ("proxy-authorization", "Basic x"),
            ("x-forwarded-host", "evil.test"),
            // headers an upstream may honour to change what a GET does
            ("x-http-method-override", "DELETE"),
            ("x-http-method", "DELETE"),
            ("x-method-override", "DELETE"),
            ("x-original-url", "/api/v1/session"),
            ("x-rewrite-url", "/api/v1/session"),
            ("grpc-metadata-token", "another-identity"),
            ("grpc-metadata-grpcgateway-cookie", "argocd.token=x"),
            ("impersonate-user", "admin"),
            ("impersonate-group", "system:masters"),
            ("range", "bytes=0-1"),
            ("cache-control", "no-cache"),
            ("accept", "application/json"),
            ("accept-encoding", "gzip"),
            ("if-none-match", "\"v1\""),
            ("if-modified-since", "Tue, 20 Oct 2026 00:00:00 GMT"),
            ("user-agent", "scarab-plugin-argocd"),
        ] {
            h.append(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let out = forwardable_request_headers(&h);
        let names: Vec<&str> = out.keys().map(HeaderName::as_str).collect();
        // `accept-language` is allowlisted, but this request's `Connection`
        // marked it hop-by-hop.
        assert_eq!(
            names,
            vec![
                "accept",
                "accept-encoding",
                "if-none-match",
                "if-modified-since",
                "user-agent"
            ]
        );
    }

    #[test]
    fn only_allowlisted_response_headers_come_back_and_never_the_credential() {
        let mut h = HeaderMap::new();
        for (name, value) in [
            ("set-cookie", "argocd.token=x"),
            ("transfer-encoding", "chunked"),
            ("content-type", "application/json"),
            ("location", "https://argo.internal/elsewhere"),
            ("www-authenticate", "Bearer realm=\"argo.internal\""),
            ("x-session-token", "fresh-session"),
            ("etag", "\"argo-api-token-s3cr3t\""),
            ("retry-after", "3"),
        ] {
            h.append(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let out = forwardable_response_headers(&h, b"argo-api-token-s3cr3t");
        let names: Vec<&str> = out.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, vec!["content-type", "retry-after"]);
    }

    #[test]
    fn a_body_is_recognised_by_its_framing() {
        let mut h = HeaderMap::new();
        assert!(!has_body(&h));
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        assert!(!has_body(&h));
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("3"));
        assert!(has_body(&h));
        let mut h = HeaderMap::new();
        h.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        assert!(has_body(&h));
    }

    #[test]
    fn the_slot_and_path_are_split_raw() {
        assert_eq!(
            split_connection_path(&format!(
                "{BROKER_PREFIX}/connections/argocd/api/v1/applications/a%2Fb"
            )),
            Some(("argocd", "/api/v1/applications/a%2Fb".to_string()))
        );
        assert_eq!(
            split_connection_path(&format!("{BROKER_PREFIX}/connections/argocd")),
            None
        );
    }

    #[test]
    fn a_ca_that_is_not_pem_is_refused() {
        assert!(upstream_client(Some("not a certificate")).is_err());
        assert!(upstream_client(None).is_ok());
    }

    /// The cut flag decides how the body ends, whatever was buffered: a
    /// Plugin that drains the buffer after a cut still ends on an error.
    #[tokio::test]
    async fn a_cut_stream_ends_in_an_error_after_the_buffer_drains() {
        for was_cut in [false, true] {
            let (tx, rx) = mpsc::channel(STREAM_BUFFER_CHUNKS);
            for _ in 0..STREAM_BUFFER_CHUNKS {
                tx.try_send(Ok(Bytes::from_static(b"x"))).unwrap();
            }
            let cut = Arc::new(AtomicBool::new(false));
            cut.store(was_cut, Ordering::SeqCst);
            drop(tx);
            let items: Vec<_> = body_stream(rx, cut).collect().await;
            assert_eq!(
                items.iter().filter(|i| i.is_ok()).count(),
                STREAM_BUFFER_CHUNKS
            );
            assert_eq!(
                items.last().is_some_and(Result::is_err),
                was_cut,
                "cut = {was_cut}"
            );
        }
    }
}
