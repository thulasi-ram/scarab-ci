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
//! - **response bodies are read for the credential**: a body that holds it
//!   is cut before the chunk that completes it, however the chunks split
//!   it, and no `Accept-Encoding` goes upstream so a body is never
//!   compressed past reading (a compressed one is refused);
//! - **destinations are pinned**: the bound host is resolved once per
//!   connection and only an address that is not link-local or loopback is
//!   connected to;
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
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
// Tokio's clock, so a test can pause and advance it past a cache's limits.
use tokio::time::Instant;

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
/// Proxied requests one installation may start per minute in this process,
/// refilled evenly (a token bucket of this size). A reporter's steady
/// traffic is a few reads a second.
pub const PROXY_REQUESTS_PER_MINUTE_PER_INSTALLATION: u32 = 600;
/// Proxied requests every installation together may have in flight in this
/// process.
pub const PROXY_CONCURRENCY_TOTAL: usize = 256;
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
/// Why a stream is cut when the upstream's body holds the credential.
const CREDENTIAL_IN_BODY: &str = "cut: the upstream response held the credential";
/// Response chunks buffered between the upstream and the Plugin.
const STREAM_BUFFER_CHUNKS: usize = 16;
/// An upstream client unused this long is dropped; the next request for its
/// CA builds a new one.
const CLIENT_IDLE: Duration = Duration::from_secs(10 * 60);
/// A bound credential is read from the secret store again once its cached
/// value is this old, used or not. A re-bind or an unbind drops the cached
/// value at once; this limit covers a secret rotated or deleted in the
/// store itself, which the broker is not told about. A secret gone from the
/// store is refused (`credential missing`) from the next read on, never
/// served from the old value.
pub const CREDENTIAL_MAX_AGE: Duration = Duration::from_secs(60);

/// The Plugin's headers that may travel upstream — content negotiation,
/// conditional reads and an identifying agent, nothing that could change
/// what the upstream does or as whom. (No `Range` or `Cache-Control`: the
/// Argo CD reporter needs neither. No `Accept-Encoding`: the broker reads
/// every response body for the credential, and cannot read a compressed
/// one.)
pub const REQUEST_HEADERS: [&str; 5] = [
    "accept",
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

/// One of an installation's [`PROXY_CONCURRENCY_PER_INSTALLATION`] permits
/// and one of the broker's [`PROXY_CONCURRENCY_TOTAL`], taken in
/// authentication and held until the response stream ends.
#[derive(Clone)]
pub(crate) struct ProxyPermit(
    #[allow(dead_code)] Arc<(OwnedSemaphorePermit, OwnedSemaphorePermit)>,
);

/// What the proxy route shares across requests: the registry on the
/// broker's pool, the journal queue, the per-installation permits and the
/// upstream clients (one per distinct CA).
pub struct BrokerProxy {
    store: Arc<dyn PluginStore>,
    journal: Option<mpsc::Sender<BrokerJournalEntry>>,
    /// Refusals not journaled because the queue was over half full.
    refusals_unjournaled: AtomicU64,
    permits: Mutex<InstallationPermits>,
    /// The broker-wide in-flight limit.
    total: Arc<Semaphore>,
    clients: Mutex<IdleCache<reqwest::Client>>,
    /// Bound credentials by `credential_ref`, so a request does not read
    /// the secret store each time; each is read again once it is
    /// [`CREDENTIAL_MAX_AGE`] old.
    credentials: Mutex<IdleCache<Arc<Vec<u8>>>>,
    /// Whether upstream connections may go to loopback (tests only).
    loopback_allowed: bool,
}

/// Why a request was refused before anything else ran.
#[derive(Debug, PartialEq, Eq)]
enum Limit {
    /// The installation's in-flight limit.
    Concurrency,
    /// The installation's per-minute limit.
    Rate,
    /// The broker-wide in-flight limit.
    Total,
}

/// Each installation's in-flight permits and request budget. An
/// installation with nothing in flight and a full budget is dropped from
/// the map whenever another takes a permit, so an uninstalled or idle
/// installation costs nothing.
#[derive(Default)]
struct InstallationPermits {
    by_installation: HashMap<String, InstallationGate>,
}

struct InstallationGate {
    in_flight: Arc<Semaphore>,
    budget: RequestBudget,
}

/// A token bucket: [`PROXY_REQUESTS_PER_MINUTE_PER_INSTALLATION`] requests,
/// refilled evenly over a minute.
struct RequestBudget {
    tokens: f64,
    at: Instant,
}

impl RequestBudget {
    const SIZE: f64 = PROXY_REQUESTS_PER_MINUTE_PER_INSTALLATION as f64;

    fn full(now: Instant) -> Self {
        Self {
            tokens: Self::SIZE,
            at: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * Self::SIZE / 60.0).min(Self::SIZE);
        self.at = now;
    }
}

impl InstallationPermits {
    /// One of `installation_id`'s permits, taken from its budget. The
    /// permit is taken while the map is held, so no permit is ever taken
    /// from a semaphore the map has just dropped (which would let a second
    /// semaphore hand out a second set of permits). A request refused for
    /// concurrency spends no budget.
    fn try_take(
        &mut self,
        installation_id: &str,
        now: Instant,
    ) -> Result<OwnedSemaphorePermit, Limit> {
        self.by_installation.retain(|id, gate| {
            gate.budget.refill(now);
            id == installation_id
                || gate.in_flight.available_permits() < PROXY_CONCURRENCY_PER_INSTALLATION
                || gate.budget.tokens < RequestBudget::SIZE
        });
        let gate = self
            .by_installation
            .entry(installation_id.to_string())
            .or_insert_with(|| InstallationGate {
                in_flight: Arc::new(Semaphore::new(PROXY_CONCURRENCY_PER_INSTALLATION)),
                budget: RequestBudget::full(now),
            });
        gate.budget.refill(now);
        if gate.budget.tokens < 1.0 {
            return Err(Limit::Rate);
        }
        let permit = gate
            .in_flight
            .clone()
            .try_acquire_owned()
            .map_err(|_| Limit::Concurrency)?;
        gate.budget.tokens -= 1.0;
        Ok(permit)
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.by_installation.len()
    }
}

/// Values by key, each dropped once it has gone unused for `idle` or is
/// `max_age` old, whichever comes first. Stale entries go whenever a value
/// is inserted.
struct IdleCache<V> {
    idle: Duration,
    max_age: Duration,
    entries: HashMap<String, CacheEntry<V>>,
}

struct CacheEntry<V> {
    value: V,
    inserted: Instant,
    used: Instant,
}

impl<V> CacheEntry<V> {
    fn fresh(&self, now: Instant, idle: Duration, max_age: Duration) -> bool {
        now.saturating_duration_since(self.used) < idle
            && now.saturating_duration_since(self.inserted) < max_age
    }
}

impl<V: Clone> IdleCache<V> {
    /// Entries go once unused for `idle`, however old.
    fn new(idle: Duration) -> Self {
        Self {
            idle,
            max_age: Duration::MAX,
            entries: HashMap::new(),
        }
    }

    /// Entries go once `max_age` old, however often used.
    fn aged(max_age: Duration) -> Self {
        Self {
            idle: max_age,
            max_age,
            entries: HashMap::new(),
        }
    }

    fn get(&mut self, key: &str, now: Instant) -> Option<V> {
        let (idle, max_age) = (self.idle, self.max_age);
        match self.entries.get_mut(key) {
            Some(entry) if entry.fresh(now, idle, max_age) => {
                entry.used = now;
                Some(entry.value.clone())
            }
            _ => None,
        }
    }

    fn insert(&mut self, key: String, value: V, now: Instant) {
        let (idle, max_age) = (self.idle, self.max_age);
        self.entries
            .retain(|_, entry| entry.fresh(now, idle, max_age));
        self.entries.insert(
            key,
            CacheEntry {
                value,
                inserted: now,
                used: now,
            },
        );
    }

    fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Stops the journal writer at shutdown, once it has written what is
/// queued. Dropping it stops nothing: the writer then runs until the proxy
/// is gone.
pub struct BrokerJournalDrain {
    stop: oneshot::Sender<()>,
    writer: tokio::task::JoinHandle<()>,
}

impl BrokerJournalDrain {
    /// Write every journal entry queued so far, then stop the writer. An
    /// entry queued after this is called is not written. Waits at most
    /// `limit`; `false` if the writer had not finished by then.
    pub async fn finish(self, limit: Duration) -> bool {
        let _ = self.stop.send(());
        tokio::time::timeout(limit, self.writer).await.is_ok()
    }
}

impl BrokerProxy {
    /// A proxy over `store`, with a `journal_queue`-entry journal queue and
    /// its writer spawned on the current Tokio runtime. Without a runtime
    /// there is no writer, and the proxy refuses every request (503): it
    /// never forwards what it cannot journal.
    pub fn spawn(store: Arc<dyn PluginStore>, journal_queue: usize) -> Arc<Self> {
        Self::spawn_with_drain(store, journal_queue, false).0
    }

    /// [`spawn`](Self::spawn), and the handle that writes the queued
    /// journal at shutdown (`None` without a runtime). `loopback_allowed`
    /// lets upstream connections go to loopback, for a test's fake upstream.
    pub fn spawn_with_drain(
        store: Arc<dyn PluginStore>,
        journal_queue: usize,
        loopback_allowed: bool,
    ) -> (Arc<Self>, Option<BrokerJournalDrain>) {
        let mut drain = None;
        let journal = tokio::runtime::Handle::try_current().ok().map(|rt| {
            let (tx, rx) = mpsc::channel(journal_queue.max(2));
            let (stop, stopped) = oneshot::channel();
            let writer = rt.spawn(write_journal(store.clone(), rx, stopped));
            drain = Some(BrokerJournalDrain { stop, writer });
            tx
        });
        if journal.is_none() {
            tracing::error!(
                "broker: no Tokio runtime for the journal writer; the read proxy is off"
            );
        }
        let proxy = Arc::new(Self {
            store,
            journal,
            refusals_unjournaled: AtomicU64::new(0),
            permits: Mutex::new(InstallationPermits::default()),
            total: Arc::new(Semaphore::new(PROXY_CONCURRENCY_TOTAL)),
            clients: Mutex::new(IdleCache::new(CLIENT_IDLE)),
            credentials: Mutex::new(IdleCache::aged(CREDENTIAL_MAX_AGE)),
            loopback_allowed,
        });
        (proxy, drain)
    }

    /// The broker-wide permit first, so a request the broker is too busy
    /// for spends none of its installation's budget.
    fn try_permit(&self, installation_id: &str) -> Result<ProxyPermit, Limit> {
        let total = self
            .total
            .clone()
            .try_acquire_owned()
            .map_err(|_| Limit::Total)?;
        let own = self
            .permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .try_take(installation_id, Instant::now())?;
        Ok(ProxyPermit(Arc::new((own, total))))
    }

    fn cached_credential(&self, credential_ref: &str) -> Option<Arc<Vec<u8>>> {
        self.credentials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(credential_ref, Instant::now())
    }

    fn cache_credential(&self, credential_ref: &str, value: Arc<Vec<u8>>) {
        self.credentials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(credential_ref.to_string(), value, Instant::now());
    }

    /// Drop a credential whose binding is gone (re-bound or unbound).
    fn forget_credential(&self, credential_ref: &str) {
        self.credentials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(credential_ref);
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
        match tx.try_send(entry) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => tracing::info!(
                "broker: the journal was closed for shutdown; an open stream's end row was not \
                 written"
            ),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::error!("broker: journal queue full; an end row was dropped")
            }
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
        let now = Instant::now();
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(client) = clients.get(&key, now) {
            return Ok(client);
        }
        let client = upstream_client_with(ca_pem, self.loopback_allowed)?;
        clients.insert(key, client.clone(), now);
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
            Ok(Some(i)) if i.held_by_the_runtime() => {
                return Err(Recheck::Decided(
                    "cut: the plugin runtime held the installation",
                ))
            }
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
            Ok(_) => {
                self.forget_credential(credential_ref);
                Err(Recheck::Decided(
                    "cut: the connection was re-bound or unbound",
                ))
            }
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
/// store or a secret (`broker::authenticate`). Over the installation's
/// in-flight or per-minute limit the request (`method uri_path`) is refused
/// with a 429, over the broker-wide limit with a 503 — journaled like any
/// refusal.
pub(crate) async fn take_permit(
    st: &AppState,
    proxy: &BrokerProxy,
    installation_id: &str,
    method: &str,
    uri_path: &str,
) -> Result<ProxyPermit, Response> {
    let limit = match proxy.try_permit(installation_id) {
        Ok(permit) => return Ok(permit),
        Err(limit) => limit,
    };
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
    let refusal = match limit {
        Limit::Concurrency => Refusal::new(
            StatusCode::TOO_MANY_REQUESTS,
            "concurrency",
            format!(
                "at most {PROXY_CONCURRENCY_PER_INSTALLATION} broker requests per installation \
                 at once"
            ),
        )
        .retry_after(RETRY_AFTER_TOO_MANY_SECS),
        Limit::Rate => Refusal::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate",
            format!(
                "at most {PROXY_REQUESTS_PER_MINUTE_PER_INSTALLATION} broker requests per \
                 installation per minute"
            ),
        )
        .retry_after(RETRY_AFTER_TOO_MANY_SECS),
        Limit::Total => Refusal::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "broker busy",
            "the broker is busy; retry",
        )
        .retry_after(RETRY_AFTER_BUSY_SECS),
    };
    proxy.journal_refusal(draft.refused(refusal.status, refusal.reason));
    Err(refusal.respond())
}

/// An upstream client for one connection: no redirects (a 3xx goes back to
/// the Plugin, so the credential is never sent where the admin did not bind
/// it), no environment proxy (the destination is exactly the bound one), and
/// — with a CA — that CA as the ONLY trust anchor. There is no skip-verify.
/// Also what the admin API uses to check a CA before binding it.
pub(crate) fn upstream_client(ca_pem: Option<&str>) -> Result<reqwest::Client, String> {
    upstream_client_with(ca_pem, false)
}

/// [`upstream_client`], resolving through [`UpstreamResolver`]: it connects
/// only to the addresses it resolved, and never to a link-local address
/// (or to loopback, unless `loopback_allowed`).
fn upstream_client_with(
    ca_pem: Option<&str>,
    loopback_allowed: bool,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
        .dns_resolver(UpstreamResolver { loopback_allowed });
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

/// AWS's instance metadata service on IPv6 (an address in the private
/// fd00::/8 range, so not caught by the link-local rule).
const AWS_METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

/// May the broker connect to `ip`? Never to link-local (169.254.0.0/16 and
/// fe80::/10, where cloud metadata services answer), the unspecified,
/// broadcast or multicast addresses, or AWS's IPv6 metadata address; to
/// loopback only when `loopback_allowed`. Private ranges are allowed: the
/// upstream is usually in the same cluster. An IPv6 address that carries an
/// IPv4 address a translator would reach (IPv4-mapped `::ffff:0:0/96`, the
/// NAT64 prefixes `64:ff9b::/96` and `64:ff9b:1::/48`, the IPv4 address in
/// the last 32 bits) is judged as that IPv4 address; the deprecated
/// IPv4-compatible `::/96` is refused outright.
fn upstream_address_allowed(ip: IpAddr, loopback_allowed: bool) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let last32 = || std::net::Ipv4Addr::from(((s[6] as u32) << 16) | s[7] as u32);
            if let Some(v4) = v6.to_ipv4_mapped() {
                IpAddr::V4(v4)
            } else if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] || s[..3] == [0x64, 0xff9b, 1] {
                IpAddr::V4(last32())
            } else if s[..6] == [0; 6] && !v6.is_loopback() && !v6.is_unspecified() {
                return false;
            } else {
                ip
            }
        }
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (v4.is_loopback() && !loopback_allowed))
        }
        IpAddr::V6(v6) => {
            !((v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6 == AWS_METADATA_V6
                || (v6.is_loopback() && !loopback_allowed))
        }
    }
}

/// A destination the broker will not connect to.
#[derive(Debug)]
struct DeniedDestination(String);

impl std::fmt::Display for DeniedDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is or resolves only to addresses the broker does not connect to \
             (link-local, loopback or metadata)",
            self.0
        )
    }
}

impl std::error::Error for DeniedDestination {}

/// Resolves a bound host once for each new connection, and hands the
/// connector only the addresses [`upstream_address_allowed`] accepts, so
/// the address checked is the address connected to: no second lookup a
/// DNS answer could change in between.
#[derive(Debug, Clone, Copy)]
struct UpstreamResolver {
    loopback_allowed: bool,
}

impl reqwest::dns::Resolve for UpstreamResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let loopback_allowed = self.loopback_allowed;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let kept: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| upstream_address_allowed(a.ip(), loopback_allowed))
                .collect();
            if kept.is_empty() {
                return Err(Box::new(DeniedDestination(host)) as _);
            }
            Ok(Box::new(kept.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Did `e` come from a [`DeniedDestination`]?
fn denied_destination(e: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(s) = source {
        if s.is::<DeniedDestination>() {
            return true;
        }
        source = s.source();
    }
    false
}

fn denied_destination_refusal(slot: &str) -> Refusal {
    Refusal::new(
        StatusCode::BAD_GATEWAY,
        "destination denied",
        format!(
            "connection `{slot}`: its destination is or resolves only to addresses the broker \
             does not connect to"
        ),
    )
}

/// Drain the journal queue into the store, a batch per round trip, until
/// the proxy is gone or `stop` fires. On `stop` the queue is closed and
/// what it holds is written before the writer returns; a dropped `stop`
/// changes nothing.
async fn write_journal(
    store: Arc<dyn PluginStore>,
    mut rx: mpsc::Receiver<BrokerJournalEntry>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut batch = Vec::with_capacity(JOURNAL_BATCH);
    let mut stoppable = true;
    loop {
        let first = tokio::select! {
            stopped = &mut stop, if stoppable => {
                if stopped.is_ok() {
                    break;
                }
                stoppable = false;
                continue;
            }
            next = rx.recv() => match next {
                Some(entry) => entry,
                None => return,
            },
        };
        batch.push(first);
        fill_batch(&mut rx, &mut batch);
        write_batch(store.as_ref(), &mut batch).await;
    }
    // Shutdown: refuse new entries, then write what the queue holds. A
    // read that reserved its slot before the close (it may be waiting on
    // its upstream, credential already sent) still sends its row: `recv`
    // returns `None` only once the queue is empty and every reserved slot
    // has sent or been dropped. `try_recv` would answer `Empty` while one
    // is outstanding, and that row would be lost.
    rx.close();
    while let Some(entry) = rx.recv().await {
        batch.push(entry);
        fill_batch(&mut rx, &mut batch);
        write_batch(store.as_ref(), &mut batch).await;
    }
}

/// Top `batch` up from the queue without waiting.
fn fill_batch(rx: &mut mpsc::Receiver<BrokerJournalEntry>, batch: &mut Vec<BrokerJournalEntry>) {
    while batch.len() < JOURNAL_BATCH {
        match rx.try_recv() {
            Ok(entry) => batch.push(entry),
            Err(_) => break,
        }
    }
}

/// Write `batch` and empty it. A failed write is retried once; then the
/// batch is logged as lost.
async fn write_batch(store: &dyn PluginStore, batch: &mut Vec<BrokerJournalEntry>) {
    if let Err(first) = store.append_broker_journal(batch).await {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Err(again) = store.append_broker_journal(batch).await {
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
    journal_slot.send(draft.head(status));

    let cut = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(STREAM_BUFFER_CHUNKS);
    let pump = Pump {
        proxy: proxy.clone(),
        installation_id: installation.id.clone(),
        slot: slot.name.clone(),
        credential_ref: connection.credential_ref.clone(),
        credential: credential.into_bytes(),
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
    // An IP literal never reaches the resolver: check it here.
    if let Some(ip) = url.host_str().and_then(|h| {
        h.trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .ok()
    }) {
        if !upstream_address_allowed(ip, proxy.loopback_allowed) {
            tracing::warn!(installation = %installation_id, slot = %slot.name, %ip,
                "broker: the bound destination is an address the broker does not connect to");
            return Err(denied_destination_refusal(&slot.name));
        }
    }
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
        Ok(Ok(resp)) => {
            // The body is read for the credential, which a compressed body
            // would hide. The broker asks for none (no Accept-Encoding goes
            // upstream), so an encoded body is an upstream ignoring that.
            if response_is_encoded(resp.headers()) {
                tracing::warn!(installation = %installation_id, slot = %slot.name,
                    "broker: the upstream encoded its response; refused");
                return Err(Refusal::new(
                    StatusCode::BAD_GATEWAY,
                    "encoded body",
                    format!(
                        "connection `{}`: the upstream compressed its response, which the broker \
                         cannot check; the broker asks for none",
                        slot.name
                    ),
                ));
            }
            Ok((connection, credential, resp))
        }
        Ok(Err(e)) if denied_destination(&e) => {
            tracing::warn!(installation = %installation_id, slot = %slot.name, error = %e,
                "broker: the bound destination resolves only to addresses the broker does not \
                 connect to");
            Err(denied_destination_refusal(&slot.name))
        }
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
        if let Some(credential) = proxy.cached_credential(&connection.credential_ref) {
            return Ok((connection, credential.as_ref().clone()));
        }
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
            Ok(secret) => {
                proxy.cache_credential(&connection.credential_ref, Arc::new(secret.value.clone()));
                return Ok((connection, secret.value));
            }
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
    /// The bound credential: a body that holds it is cut before the chunk
    /// that completes it ([`scan_for_credential`]).
    credential: Vec<u8>,
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
        // The tail of what arrived that could be the start of the
        // credential: held back until the next chunk shows it is not.
        let mut held = Vec::new();
        let end = loop {
            tokio::select! {
                _ = recheck.tick() => {
                    if let Err(why) = self.check().await {
                        break Some(self.cut(&tx, why).await);
                    }
                }
                _ = tx.closed() => break None,
                next = body.next() => match next {
                    None => {
                        // The body ended, so what was held back is not the
                        // credential.
                        if held.is_empty() {
                            break None;
                        }
                        let rest = Bytes::from(std::mem::take(&mut held));
                        let len = rest.len() as u64;
                        match self.deliver(&tx, rest, &mut recheck).await {
                            Ok(()) => bytes += len,
                            Err(Some(why)) => break Some(why),
                            Err(None) => {}
                        }
                        break None;
                    }
                    Some(Err(e)) => {
                        tracing::warn!(installation = %self.installation_id, slot = %self.slot,
                            error = %e, "broker: upstream body failed mid-stream");
                        self.cut.store(true, Ordering::SeqCst);
                        send_error(&tx, "the upstream response failed").await;
                        break Some("upstream body error");
                    }
                    Some(Ok(chunk)) => {
                        let Some(ready) = scan_for_credential(&mut held, &chunk, &self.credential)
                        else {
                            break Some(self.cut(&tx, CREDENTIAL_IN_BODY).await);
                        };
                        if ready.is_empty() {
                            continue;
                        }
                        let len = ready.len() as u64;
                        match self.deliver(&tx, ready, &mut recheck).await {
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

/// Does the response declare any coding but `identity`, in any of its
/// `Content-Encoding` headers or any comma-separated value of one? A value
/// that is not text counts as a coding.
fn response_is_encoded(headers: &HeaderMap) -> bool {
    headers.get_all(header::CONTENT_ENCODING).iter().any(|v| {
        v.to_str().map_or(true, |v| {
            v.split(',')
                .map(str::trim)
                .any(|coding| !coding.is_empty() && !coding.eq_ignore_ascii_case("identity"))
        })
    })
}

/// Add `chunk` to what was `held` back and return what may go to the
/// Plugin, or `None` if the credential is in it. What is held back is the
/// longest tail that is a start of the credential, so a credential split
/// across chunks is caught and no part of it is sent first; any other tail
/// goes at once, so a watch stream's events are not delayed.
fn scan_for_credential(held: &mut Vec<u8>, chunk: &[u8], credential: &[u8]) -> Option<Bytes> {
    held.extend_from_slice(chunk);
    if contains(held, credential) {
        held.clear();
        return None;
    }
    let longest = credential.len().saturating_sub(1).min(held.len());
    let keep = (1..=longest)
        .rev()
        .find(|&k| held.ends_with(&credential[..k]))
        .unwrap_or(0);
    let ready: Vec<u8> = held.drain(..held.len() - keep).collect();
    Some(Bytes::from(ready))
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

    /// git-bug 4711823 (3): a credential in the body is caught however the
    /// chunks split it, and no part of it is handed on first; a tail that
    /// only looks like its start is handed on once the next chunk shows it
    /// is not.
    #[test]
    fn a_credential_in_the_body_is_caught_across_chunks() {
        let credential = b"argo-api-token-s3cr3t";
        let body = b"{\"token\":\"argo-api-token-s3cr3t\"}";
        for split in 1..body.len() {
            for second in split..body.len() {
                let mut held = Vec::new();
                let mut sent = Vec::new();
                let mut caught = false;
                for chunk in [&body[..split], &body[split..second], &body[second..]] {
                    match scan_for_credential(&mut held, chunk, credential) {
                        Some(ready) => sent.extend_from_slice(&ready),
                        None => {
                            caught = true;
                            break;
                        }
                    }
                }
                assert!(caught, "split at {split} and {second}");
                assert!(
                    b"{\"token\":\"".starts_with(&sent),
                    "split at {split} and {second}: sent {:?}",
                    String::from_utf8_lossy(&sent)
                );
            }
        }

        // `argo-` might start the credential; `argo-cd` does not.
        let mut held = Vec::new();
        let first = scan_for_credential(&mut held, b"{\"app\":\"argo-", credential).unwrap();
        assert_eq!(&first[..], b"{\"app\":\"");
        let second = scan_for_credential(&mut held, b"cd\"}\n", credential).unwrap();
        assert_eq!(&second[..], b"argo-cd\"}\n");
        assert!(held.is_empty());
    }

    /// git-bug 4711823 (3): every Content-Encoding header and every value
    /// in one is checked.
    #[test]
    fn every_content_encoding_value_is_checked() {
        let headers = |values: &[&'static str]| {
            let mut h = HeaderMap::new();
            for v in values {
                h.append(header::CONTENT_ENCODING, HeaderValue::from_static(v));
            }
            h
        };
        assert!(!response_is_encoded(&headers(&[])));
        assert!(!response_is_encoded(&headers(&["identity"])));
        assert!(!response_is_encoded(&headers(&["Identity"])));
        assert!(response_is_encoded(&headers(&["gzip"])));
        assert!(response_is_encoded(&headers(&["identity", "gzip"])));
        assert!(response_is_encoded(&headers(&["identity, gzip"])));
        assert!(response_is_encoded(&headers(&["br"])));
    }

    /// git-bug 4711823 (4): which addresses the broker connects to.
    #[test]
    fn the_broker_never_connects_to_link_local_or_loopback() {
        let cases: [(&str, bool); 21] = [
            ("169.254.169.254", false), // cloud metadata
            ("169.254.0.1", false),
            ("fe80::1", false),
            ("febf::1", false),
            ("::ffff:169.254.169.254", false),
            ("fd00:ec2::254", false), // AWS metadata over IPv6
            ("0.0.0.0", false),
            ("::", false),
            ("255.255.255.255", false),
            ("224.0.0.1", false),
            ("127.0.0.1", false),
            ("::1", false),
            ("::ffff:127.0.0.1", false),
            ("10.0.0.5", true), // in-cluster upstreams are private
            ("192.168.1.10", true),
            ("fd12:3456::1", true),
            ("64:ff9b::a9fe:a9fe", false),   // NAT64 → 169.254.169.254
            ("64:ff9b:1::a9fe:a9fe", false), // local-use NAT64 prefix
            ("::a9fe:a9fe", false),          // IPv4-compatible (deprecated)
            ("::10.0.0.5", false),
            ("64:ff9b::a00:5", true), // NAT64 → 10.0.0.5
        ];
        for (ip, allowed) in cases {
            assert_eq!(
                upstream_address_allowed(ip.parse().unwrap(), false),
                allowed,
                "{ip}"
            );
        }
        assert!(upstream_address_allowed("127.0.0.1".parse().unwrap(), true));
        assert!(upstream_address_allowed("8.8.8.8".parse().unwrap(), false));
        assert!(!upstream_address_allowed(
            "169.254.169.254".parse().unwrap(),
            true
        ));
    }

    /// git-bug 4711823 (6): an installation with nothing in flight leaves
    /// the permit map when another takes a permit; one with a permit out
    /// stays, and its limit holds.
    #[test]
    fn idle_installations_leave_the_permit_map() {
        // A minute apart, so each installation's budget is full again.
        let t0 = Instant::now();
        let t = |minutes: u64| t0 + Duration::from_secs(60 * minutes);
        let mut permits = InstallationPermits::default();
        let a = permits.try_take("a", t(0)).expect("a's first permit");
        drop(permits.try_take("b", t(1)).expect("b's first permit"));
        assert_eq!(permits.tracked(), 2, "a holds a permit, b was just used");

        drop(permits.try_take("c", t(2)).expect("c's first permit"));
        assert_eq!(permits.tracked(), 2, "b had nothing in flight: dropped");

        drop(a);
        drop(permits.try_take("d", t(3)).expect("d's first permit"));
        assert_eq!(permits.tracked(), 1, "only d, just used, is left");

        let held: Vec<_> = (0..PROXY_CONCURRENCY_PER_INSTALLATION)
            .map(|_| permits.try_take("e", t(4)).expect("within e's limit"))
            .collect();
        drop(permits.try_take("f", t(4)));
        assert_eq!(
            permits.try_take("e", t(4)).err(),
            Some(Limit::Concurrency),
            "e's limit survives pruning"
        );
        drop(held);
    }

    /// git-bug 4711823 (2): an installation may start
    /// PROXY_REQUESTS_PER_MINUTE_PER_INSTALLATION requests in a burst, then
    /// one more each tenth of a second; another installation's budget is
    /// its own; a request refused for concurrency spends nothing.
    #[test]
    fn an_installation_starts_at_most_its_requests_per_minute() {
        let t0 = Instant::now();
        let mut permits = InstallationPermits::default();
        for n in 0..PROXY_REQUESTS_PER_MINUTE_PER_INSTALLATION {
            drop(
                permits
                    .try_take("a", t0)
                    .unwrap_or_else(|l| panic!("request {n}: {l:?}")),
            );
        }
        assert_eq!(permits.try_take("a", t0).err(), Some(Limit::Rate));
        assert!(permits.try_take("b", t0).is_ok(), "b's budget is its own");

        let tick =
            Duration::from_millis(60_000 / PROXY_REQUESTS_PER_MINUTE_PER_INSTALLATION as u64);
        drop(permits.try_take("a", t0 + tick).expect("refilled by one"));
        assert_eq!(permits.try_take("a", t0 + tick).err(), Some(Limit::Rate));

        // At the in-flight limit, refusals spend no budget: c is left one
        // request once it holds every permit, and still has it after.
        let t1 = t0 + Duration::from_secs(120);
        let spare = PROXY_REQUESTS_PER_MINUTE_PER_INSTALLATION as usize
            - PROXY_CONCURRENCY_PER_INSTALLATION
            - 1;
        for _ in 0..spare {
            drop(permits.try_take("c", t1).expect("within c's budget"));
        }
        let held: Vec<_> = (0..PROXY_CONCURRENCY_PER_INSTALLATION)
            .map(|_| permits.try_take("c", t1).expect("within c's limit"))
            .collect();
        for _ in 0..1000 {
            assert_eq!(permits.try_take("c", t1).err(), Some(Limit::Concurrency));
        }
        drop(held);
        assert!(
            permits.try_take("c", t1).is_ok(),
            "c's last request was not spent"
        );
        assert_eq!(permits.try_take("c", t1).err(), Some(Limit::Rate));
    }

    /// git-bug 4711823 (6): a cached value unused for its idle time goes
    /// when the next value is inserted, and a used one stays.
    #[test]
    fn an_idle_cache_drops_what_went_unused() {
        let start = Instant::now();
        let mut cache = IdleCache::new(Duration::from_secs(60));
        cache.insert("old".into(), 1, start);
        cache.insert("used".into(), 2, start);
        assert_eq!(cache.get("used", start + Duration::from_secs(50)), Some(2));

        cache.insert("new".into(), 3, start + Duration::from_secs(70));
        assert_eq!(cache.len(), 2, "`old` went unused for 70s: dropped");
        assert_eq!(cache.get("old", start + Duration::from_secs(70)), None);
        assert_eq!(cache.get("used", start + Duration::from_secs(70)), Some(2));
        assert_eq!(
            cache.get("new", start + Duration::from_secs(200)),
            None,
            "an entry past its idle time is not served even before it is dropped"
        );
    }

    /// An aged cache serves a value until it is `max_age` old, however
    /// often it was used, and then not at all.
    #[test]
    fn an_aged_cache_drops_a_value_at_its_age_even_while_used() {
        let start = Instant::now();
        let mut cache = IdleCache::aged(Duration::from_secs(60));
        cache.insert("cred".into(), 1, start);
        for secs in [10, 20, 30, 40, 50, 59] {
            assert_eq!(
                cache.get("cred", start + Duration::from_secs(secs)),
                Some(1)
            );
        }
        assert_eq!(cache.get("cred", start + Duration::from_secs(60)), None);
        cache.insert("other".into(), 2, start + Duration::from_secs(60));
        assert_eq!(cache.len(), 1, "the aged value is dropped on insert");
    }

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
        // marked it hop-by-hop. `accept-encoding` is not: a compressed body
        // could not be read for the credential.
        assert_eq!(
            names,
            vec!["accept", "if-none-match", "if-modified-since", "user-agent"]
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
