//! The CredentialBroker's read proxy (ADR-0072 §5, MMP slice d3):
//! `GET|HEAD /_scarab/plugin/v1/connections/{slot}/{*path}`.
//!
//! For each request a Plugin sends, the broker
//!
//! 1. checks it against the manifest's **read rules**
//!    ([`scarab_plugin::match_read_request`]: decoded before matching, `*` =
//!    one segment, traversal and encoded separators refused);
//! 2. adds the credential — `Authorization: Bearer <secret>` — after
//!    dropping the Plugin's own `Authorization` (its ServiceAccount token),
//!    which therefore never leaves the cluster;
//! 3. forwards it to the PluginConnection's admin-bound `base_url`; and
//! 4. journals it, off the request path.
//!
//! **The destination is always the admin-bound `base_url`.** The upstream
//! URL is built from the binding and the matched, re-encoded path only —
//! never from the request's authority, its `Host` header or anything else a
//! Plugin sends — so a Plugin cannot aim the credential at a host of its
//! choosing. That is the core property of brokered mode (ADR-0072 §6).
//!
//! The rest follows from the Plugin being an untrusted caller:
//!
//! - **no request body** (a read carries none) and **no redirects** — a 3xx
//!   is handed back, never followed, so an upstream cannot bounce the
//!   credential elsewhere;
//! - **hop-by-hop headers** are dropped both ways, and so are credentials
//!   the admin did not bind (`Cookie`, `Proxy-Authorization`);
//! - a custom CA is the **only** trust anchor for its connection, and there
//!   is no skip-verify;
//! - responses **stream**, and every stream re-checks the kill switch (and
//!   its binding) every [`KILL_RECHECK_INTERVAL`]: a kill, an uninstall, a
//!   re-bind or an unbind cuts a stream already open within that interval,
//!   not when it happens to end;
//! - at most [`PROXY_CONCURRENCY_PER_INSTALLATION`] requests per
//!   installation are in flight in this process (429 beyond), on top of the
//!   broker's own small pool (judge-d1 P3).

use std::collections::HashMap;
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
    match_read_request, rule_label, BrokerJournalEntry, InstallationState, PluginCapability,
    PluginConnection, PluginStore, ReadRequestRefusal,
};

use super::AppState;
use crate::broker::{PluginCaller, BROKER_PREFIX};
use crate::plugins::{plugin_connection_secret_scope, PLUGIN_CONNECTION_CREDENTIALS_ORG};

/// How often an open response stream re-reads the kill switch and its
/// binding. A kill reaches an open stream within this long.
pub const KILL_RECHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Proxied requests one installation may have in flight in this process. A
/// reporter needs one long watch stream plus a few short reads.
pub const PROXY_CONCURRENCY_PER_INSTALLATION: usize = 16;
/// Upstream connect timeout, and how long the upstream has to send its
/// response head. There is no total timeout: a watch stream is meant to stay
/// open, and the kill re-check is what bounds it.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UPSTREAM_HEADERS_TIMEOUT: Duration = Duration::from_secs(30);
/// The journal queue between requests and the writer, and how many entries
/// the writer folds into one INSERT.
const JOURNAL_QUEUE: usize = 4096;
const JOURNAL_BATCH: usize = 256;
/// How much of a request's path and slot the journal keeps.
const MAX_JOURNAL_PATH_CHARS: usize = 2048;
const MAX_JOURNAL_SLOT_CHARS: usize = 63;
const RETRY_AFTER_TOO_MANY_SECS: u64 = 1;
const RETRY_AFTER_BUSY_SECS: u64 = 5;
/// Response chunks buffered between the upstream and the Plugin.
const STREAM_BUFFER_CHUNKS: usize = 16;

/// The route's path, as `broker::router` registers it.
pub fn route_path() -> String {
    format!("{BROKER_PREFIX}/connections/{{slot}}/{{*path}}")
}

/// Marks a response as the upstream's own: the broker's 500-scrubber
/// (`broker::scrub_internal_errors`) passes it through, since its body is the
/// vendor's, not an internal error of Scarab's.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UpstreamResponse;

/// What the proxy route shares across requests: the registry on the
/// broker's pool, the journal queue, the per-installation permits and the
/// upstream clients (one per distinct CA).
pub struct BrokerProxy {
    store: Arc<dyn PluginStore>,
    journal: Option<mpsc::Sender<BrokerJournalEntry>>,
    permits: Mutex<HashMap<String, Arc<Semaphore>>>,
    clients: Mutex<HashMap<String, reqwest::Client>>,
}

impl BrokerProxy {
    /// A proxy over `store`, with its journal writer spawned on the current
    /// Tokio runtime. Without a runtime there is no writer, and the proxy
    /// refuses every request (503): it never forwards what it cannot journal.
    pub fn spawn(store: Arc<dyn PluginStore>) -> Arc<Self> {
        let journal = tokio::runtime::Handle::try_current().ok().map(|rt| {
            let (tx, rx) = mpsc::channel(JOURNAL_QUEUE);
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

    /// Whether the journal can take an entry right now.
    fn journal_has_room(&self) -> bool {
        self.journal.as_ref().is_some_and(|tx| tx.capacity() > 0)
    }

    /// Queue an entry; never waits. A full queue drops it loudly — only a
    /// refusal can hit that, because a request is not forwarded unless the
    /// queue had room when it started.
    fn journal(&self, entry: BrokerJournalEntry) {
        let Some(tx) = &self.journal else {
            return;
        };
        if let Err(e) = tx.try_send(entry) {
            tracing::error!(error = %e, "broker: journal queue full; an entry was dropped");
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
    /// the connection still bound to the credential it started with? `Err`
    /// says why not. A read that fails cuts too — a kill switch that cannot
    /// be read is not proof it is off.
    async fn still_authorised(
        &self,
        installation_id: &str,
        slot: &str,
        credential_ref: &str,
    ) -> Result<(), &'static str> {
        match self.store.installation_by_id(installation_id).await {
            Ok(Some(i)) if i.state == InstallationState::Enabled => {}
            Ok(_) => return Err("cut: the installation was killed or uninstalled"),
            Err(_) => return Err("cut: the kill switch could not be read"),
        }
        match self.store.connection(installation_id, slot).await {
            Ok(Some(c)) if c.credential_ref == credential_ref => Ok(()),
            Ok(_) => Err("cut: the connection was re-bound or unbound"),
            Err(_) => Err("cut: the connection could not be read"),
        }
    }
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

/// One request's journal entry in the making.
struct Draft {
    installation_id: String,
    slot: String,
    method: String,
    path: String,
    rule: Option<String>,
    started_at: i64,
    started: Instant,
}

impl Draft {
    fn finish(
        &self,
        status: StatusCode,
        bytes: Option<u64>,
        denied_reason: Option<String>,
    ) -> BrokerJournalEntry {
        BrokerJournalEntry {
            installation_id: self.installation_id.clone(),
            slot: self.slot.clone(),
            method: self.method.clone(),
            path: self.path.clone(),
            rule: self.rule.clone(),
            status: Some(status.as_u16()),
            bytes,
            started_at: self.started_at,
            duration_ms: Some(self.started.elapsed().as_millis().min(u64::MAX as u128) as u64),
            denied_reason,
        }
    }
}

/// Bounded journal text: at most `max` characters, no NUL (Postgres TEXT
/// refuses it).
fn bounded(s: &str, max: usize) -> String {
    s.chars().filter(|c| *c != '\0').take(max).collect()
}

/// `…/connections/{slot}/{rest}` → (`slot`, `/{rest}`), both still exactly
/// as sent (percent-encoded). Axum's `Path` would decode them, and the
/// matcher must see the raw text to refuse `%2F` and friends.
fn split_connection_path(path: &str) -> Option<(&str, String)> {
    let rest = path.strip_prefix(&format!("{BROKER_PREFIX}/connections/"))?;
    let (slot, tail) = rest.split_once('/')?;
    Some((slot, format!("/{tail}")))
}

/// Headers that describe one hop, never the message (RFC 9110 §7.6.1), plus
/// the non-standard `Proxy-Connection`.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The header names a `Connection` header marks hop-by-hop.
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

/// The Plugin's headers that may travel upstream. Dropped: hop-by-hop ones;
/// `Host` (the upstream's is derived from the bound URL); `Authorization`
/// (the Plugin's own ServiceAccount token — the bound credential replaces
/// it) and `Cookie` (a credential nobody bound); the framing of a body a
/// read does not have; and forwarding metadata a Plugin could forge.
fn forwardable_request_headers(headers: &HeaderMap) -> HeaderMap {
    let listed = connection_listed(headers);
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        let dropped = HOP_BY_HOP.contains(&n)
            || listed.iter().any(|l| l == n)
            || matches!(
                n,
                "host"
                    | "authorization"
                    | "cookie"
                    | "content-length"
                    | "content-type"
                    | "expect"
                    | "forwarded"
                    | "x-real-ip"
            )
            || n.starts_with("x-forwarded-");
        if !dropped {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// The upstream's headers that may travel back: everything but hop-by-hop
/// ones and `Set-Cookie` (a session the Plugin has no use for, and could
/// not present — its `Cookie` is dropped).
fn forwardable_response_headers(headers: &HeaderMap) -> HeaderMap {
    let listed = connection_listed(headers);
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) || listed.iter().any(|l| l == n) || n == "set-cookie" {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Does the request carry (or announce) a body?
fn has_body(headers: &HeaderMap) -> bool {
    headers.contains_key(header::TRANSFER_ENCODING)
        || headers
            .get(header::CONTENT_LENGTH)
            .is_some_and(|v| v.to_str().map_or(true, |s| s.trim() != "0"))
}

/// The upstream URL: the bound base, the matched path re-encoded, the query
/// as sent. Checked to still be the bound origin under the bound path — the
/// matcher's canonical encoding already guarantees it; this makes the core
/// property hold even if that ever regressed.
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

fn text(status: StatusCode, message: String) -> Response {
    (status, message).into_response()
}

fn with_retry_after(mut resp: Response, secs: u64) -> Response {
    resp.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    resp
}

/// `GET|HEAD /_scarab/plugin/v1/connections/{slot}/{*path}` — authenticated
/// and kill-switch-checked by `broker::authenticate_plugin` before it runs.
pub(crate) async fn proxy(
    State(st): State<AppState>,
    Extension(proxy): Extension<Arc<BrokerProxy>>,
    Extension(PluginCaller(installation)): Extension<PluginCaller>,
    req: Request,
) -> Response {
    let started = Instant::now();
    let started_at = st.clock.now().await.0;
    let Some((slot_raw, raw_path)) = split_connection_path(req.uri().path()) else {
        return text(StatusCode::NOT_FOUND, "no such broker route".into());
    };
    let mut draft = Draft {
        installation_id: installation.id.clone(),
        slot: bounded(slot_raw, MAX_JOURNAL_SLOT_CHARS),
        method: bounded(req.method().as_str(), 16),
        path: bounded(&raw_path, MAX_JOURNAL_PATH_CHARS),
        rule: None,
        started_at,
        started,
    };
    // Every refusal is journaled with its reason, then answered.
    let refuse = |draft: &Draft, status: StatusCode, reason: &str, message: String| {
        proxy.journal(draft.finish(status, None, Some(reason.to_string())));
        text(status, message)
    };

    if !installation.manifest.has(PluginCapability::UseConnection) {
        return refuse(
            &draft,
            StatusCode::FORBIDDEN,
            "capability",
            format!(
                "plugin installation `{}` was not approved for use_connection",
                installation.name
            ),
        );
    }
    let Some(slot) = installation
        .manifest
        .connections
        .iter()
        .find(|s| s.name == slot_raw)
    else {
        return refuse(
            &draft,
            StatusCode::NOT_FOUND,
            "unknown slot",
            format!(
                "plugin installation `{}` declares no connection slot `{}`",
                installation.name, draft.slot
            ),
        );
    };
    if has_body(req.headers()) {
        return refuse(
            &draft,
            StatusCode::BAD_REQUEST,
            "request body",
            "a read carries no request body; the broker forwards none (ADR-0072 §5)".into(),
        );
    }
    let matched = match match_read_request(slot, req.method().as_str(), &raw_path) {
        Ok(m) => m,
        Err(refusal @ ReadRequestRefusal::MethodNotAllowed(_)) => {
            let mut resp = refuse(
                &draft,
                StatusCode::METHOD_NOT_ALLOWED,
                "method",
                format!("{refusal}; write rules are not available (ADR-0072 MMP)"),
            );
            resp.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return resp;
        }
        Err(refusal) => {
            return refuse(
                &draft,
                StatusCode::FORBIDDEN,
                "no read rule",
                refusal.to_string(),
            )
        }
    };
    draft.rule = Some(rule_label(matched.rule));

    let connection = match proxy.store.connection(&installation.id, &slot.name).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return refuse(
                &draft,
                StatusCode::CONFLICT,
                "not bound",
                format!(
                    "connection `{}` is not bound: an org admin has not set its destination and \
                     credential yet",
                    slot.name
                ),
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "broker: connection read failed");
            return with_retry_after(
                refuse(
                    &draft,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "store busy",
                    "the broker is busy; retry".into(),
                ),
                RETRY_AFTER_BUSY_SECS,
            );
        }
    };
    let Some(secrets) = st.secrets.as_deref() else {
        // A misconfiguration, not the Plugin's business: scrubbed to a
        // request id on the way out.
        return refuse(
            &draft,
            StatusCode::INTERNAL_SERVER_ERROR,
            "no secret store",
            "broker: no secret store wired".into(),
        );
    };
    let credential = match secrets
        .get(
            &plugin_connection_secret_scope(),
            &connection.credential_ref,
        )
        .await
    {
        Ok(secret) => secret.value,
        Err(scarab_secrets::SecretError::NotFound) => {
            tracing::error!(
                installation = %installation.id,
                slot = %slot.name,
                scope = PLUGIN_CONNECTION_CREDENTIALS_ORG,
                "broker: a bound connection's credential is missing from the secret store"
            );
            return refuse(
                &draft,
                StatusCode::CONFLICT,
                "credential missing",
                format!(
                    "connection `{}` has no credential: an org admin must bind it again",
                    slot.name
                ),
            );
        }
        Err(e) => {
            return refuse(
                &draft,
                StatusCode::INTERNAL_SERVER_ERROR,
                "secret store",
                format!("broker: secret store: {e}"),
            )
        }
    };
    // Validated as visible ASCII at bind time; refuse rather than send a
    // malformed header if the store holds anything else.
    let Some(bearer) = String::from_utf8(credential)
        .ok()
        .and_then(|t| HeaderValue::from_str(&format!("Bearer {t}")).ok())
    else {
        return refuse(
            &draft,
            StatusCode::INTERNAL_SERVER_ERROR,
            "credential unusable",
            "broker: a bound credential is not a valid header value".into(),
        );
    };
    let mut bearer = bearer;
    bearer.set_sensitive(true);

    let Some(permit) = proxy.try_permit(&installation.id) else {
        return with_retry_after(
            refuse(
                &draft,
                StatusCode::TOO_MANY_REQUESTS,
                "concurrency",
                format!(
                    "at most {PROXY_CONCURRENCY_PER_INSTALLATION} broker requests per \
                     installation at once"
                ),
            ),
            RETRY_AFTER_TOO_MANY_SECS,
        );
    };
    // Never forward what cannot be journaled.
    if !proxy.journal_has_room() {
        return with_retry_after(
            text(
                StatusCode::SERVICE_UNAVAILABLE,
                "the broker is busy; retry".into(),
            ),
            RETRY_AFTER_BUSY_SECS,
        );
    }
    let url = match upstream_url(&connection, &matched.upstream_path, req.uri().query()) {
        Ok(url) => url,
        Err(e) => {
            return refuse(
                &draft,
                StatusCode::INTERNAL_SERVER_ERROR,
                "destination",
                format!("broker: {e}"),
            )
        }
    };
    let client = match proxy.client(connection.ca_pem.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            return refuse(
                &draft,
                StatusCode::INTERNAL_SERVER_ERROR,
                "client",
                format!(
                    "broker: upstream client for connection `{}`: {e}",
                    slot.name
                ),
            )
        }
    };
    let mut headers = forwardable_request_headers(req.headers());
    headers.insert(header::AUTHORIZATION, bearer);
    let method = req.method().clone();
    // The request body (none, checked above) is dropped here, never read.
    drop(req);

    let upstream = match tokio::time::timeout(
        UPSTREAM_HEADERS_TIMEOUT,
        client.request(method, url).headers(headers).send(),
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            // The error text names the bound URL: an operator's business,
            // not the Plugin's.
            tracing::warn!(installation = %installation.id, slot = %slot.name, error = %e,
                "broker: upstream request failed");
            return refuse(
                &draft,
                StatusCode::BAD_GATEWAY,
                "upstream unreachable",
                format!(
                    "connection `{}`: the upstream could not be reached",
                    slot.name
                ),
            );
        }
        Err(_) => {
            return refuse(
                &draft,
                StatusCode::GATEWAY_TIMEOUT,
                "upstream timeout",
                format!(
                    "connection `{}`: the upstream sent no response within {}s",
                    slot.name,
                    UPSTREAM_HEADERS_TIMEOUT.as_secs()
                ),
            )
        }
    };

    let status = upstream.status();
    let response_headers = forwardable_response_headers(upstream.headers());
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(STREAM_BUFFER_CHUNKS);
    let pump = Pump {
        proxy: proxy.clone(),
        installation_id: installation.id.clone(),
        slot: slot.name.clone(),
        credential_ref: connection.credential_ref.clone(),
    };
    tokio::spawn(async move {
        let _permit = permit;
        let (bytes, end) = pump.run(upstream, tx).await;
        pump.proxy
            .journal(draft.finish(status, Some(bytes), end.map(str::to_string)));
    });

    let mut resp = Response::new(Body::from_stream(
        tokio_stream::wrappers::ReceiverStream::new(rx),
    ));
    *resp.status_mut() = status;
    *resp.headers_mut() = response_headers;
    resp.extensions_mut().insert(UpstreamResponse);
    resp
}

/// Moves one upstream response body to the Plugin, re-checking the kill
/// switch and the binding while it runs.
struct Pump {
    proxy: Arc<BrokerProxy>,
    installation_id: String,
    slot: String,
    credential_ref: String,
}

impl Pump {
    /// Returns the bytes delivered and, if the stream did not simply end,
    /// why (`cut: …` for a kill-switch cut).
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

    async fn check(&self) -> Result<(), &'static str> {
        self.proxy
            .still_authorised(&self.installation_id, &self.slot, &self.credential_ref)
            .await
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
        send_error(tx, why).await;
        why
    }
}

/// End the Plugin's body with an error. Waits briefly for room, so a Plugin
/// that is reading sees the error rather than a clean end; one that stopped
/// reading may instead drain what was buffered before the cut and then see
/// the body end — never anything sent after it.
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
            Some("project=prod&x=%2F"),
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://argo.acme.test/argo/api/v1/applications?project=prod&x=%2F"
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
    fn the_plugin_s_token_and_hop_by_hop_headers_never_go_upstream() {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer sa-token"),
        );
        h.insert(header::HOST, HeaderValue::from_static("evil.test"));
        h.insert(header::COOKIE, HeaderValue::from_static("session=1"));
        h.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-secret"),
        );
        h.insert("x-secret", HeaderValue::from_static("1"));
        h.insert("proxy-authorization", HeaderValue::from_static("Basic x"));
        h.insert("x-forwarded-host", HeaderValue::from_static("evil.test"));
        h.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"v1\""));
        let out = forwardable_request_headers(&h);
        let names: Vec<&str> = out.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, vec!["accept", "if-none-match"]);
    }

    #[test]
    fn set_cookie_and_hop_by_hop_headers_never_come_back() {
        let mut h = HeaderMap::new();
        h.insert(
            header::SET_COOKIE,
            HeaderValue::from_static("argocd.token=x"),
        );
        h.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h.insert(header::LOCATION, HeaderValue::from_static("/elsewhere"));
        let out = forwardable_response_headers(&h);
        let names: Vec<&str> = out.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, vec!["content-type", "location"]);
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
}
