//! The CredentialBroker's read proxy over HTTP (ADR-0072 §5, MMP slice d3):
//! an org admin binds the Argo CD plugin's `argocd` connection slot on
//! the public API, and the plugin reads through the broker's own router.
//!
//! The upstream is a real HTTP server — an in-process axum app on
//! 127.0.0.1 that records every request it gets — so what is asserted is
//! what actually crossed the wire: which credential, which headers, which
//! host, which path, and that nothing arrived at all when the broker
//! refused. Real Postgres for every store; the fakes are the token verifier
//! (it stands in for TokenReview, a true external) and the secret store
//! (`FakeSecrets`, as the forge-connection tests use). Each test gets its own
//! database and skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request as AxumRequest, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_plugin::{
    BrokerJournalEntry, BrokerJournalFilter, BrokerJournalPhase, PluginStore, PluginTokenVerifier,
    TokenRejected, VerifiedServiceAccount,
};
use scarab_secrets::{Secret, SecretProvider, SecretScope};
use scarab_server::broker_proxy::{
    CREDENTIAL_MAX_AGE, KILL_RECHECK_INTERVAL, PROXY_CONCURRENCY_PER_INSTALLATION,
};
use scarab_server::plugins::{PluginPolicy, PLUGIN_CONNECTION_CREDENTIALS_ORG};
use scarab_server::test_support::{argocd_fetcher, argocd_install_body};
use scarab_server::{broker, router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeSecrets, InMemoryDb, InMemoryObjectStore};

const CLOCK_START_MS: i64 = 1_000_000;
const REPO: &str = "ghcr.io/thulasi-ram/scarab-plugin-argocd";
const NAMESPACE: &str = "scarab-plugins";
/// What the admin binds. It must reach the upstream, and nothing else.
const ARGO_TOKEN: &str = "argo-api-token-s3cr3t";

// ---------------------------------------------------------------------------
// The fake upstream: a real HTTP server that records what reaches it.
// ---------------------------------------------------------------------------

/// One request as the upstream received it.
#[derive(Debug, Clone)]
struct Seen {
    path_and_query: String,
    headers: HeaderMap,
}

#[derive(Clone, Default)]
struct Upstream {
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Set when the watch stream's body is dropped — the upstream side of a
    /// cut stream.
    stream_dropped: Arc<AtomicBool>,
}

impl Upstream {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Sets its flag when dropped: the watch stream's body holds one.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

async fn upstream_handler(State(up): State<Upstream>, req: AxumRequest) -> Response {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    up.seen.lock().unwrap().push(Seen {
        path_and_query: path_and_query.clone(),
        headers: req.headers().clone(),
    });
    let path = req.uri().path().to_string();
    match path.as_str() {
        "/api/version" => "v2.13.0".into_response(),
        // A response that echoes the bound credential, split across two
        // chunks, after a first chunk that is clean.
        "/api/v1/applications" if path_and_query.contains("projects=echo") => {
            let token = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or_default()
                .to_string();
            let (head, tail) = token.split_at(token.len() / 2);
            let chunks: Vec<Result<axum::body::Bytes, std::io::Error>> = vec![
                Ok(axum::body::Bytes::from_static(b"{\"items\":[]}\n")),
                Ok(axum::body::Bytes::from(format!("{{\"token\":\"{head}"))),
                Ok(axum::body::Bytes::from(format!("{tail}\"}}"))),
            ];
            Body::from_stream(futures::stream::iter(chunks)).into_response()
        }
        "/api/v1/applications" if path_and_query.contains("projects=gzip") => (
            [("content-encoding", "gzip")],
            vec![0x1f_u8, 0x8b, 0x08, 0x00],
        )
            .into_response(),
        "/api/v1/applications" => (
            [
                ("content-type", "application/json"),
                ("set-cookie", "argocd.token=leak"),
            ],
            r#"{"items":[{"metadata":{"name":"web"}}]}"#,
        )
            .into_response(),
        "/api/v1/applications/redirector" => (
            StatusCode::FOUND,
            [("location", "/api/v1/applications/landing")],
        )
            .into_response(),
        "/api/v1/stream/applications" => {
            let flag = DropFlag(up.stream_dropped.clone());
            let ticks = futures::stream::unfold(flag, |flag| async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Some((
                    Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                        b"{\"type\":\"MODIFIED\"}\n",
                    )),
                    flag,
                ))
            });
            Body::from_stream(ticks).into_response()
        }
        _ => (StatusCode::OK, "upstream saw it").into_response(),
    }
}

/// Serve the fake upstream on an ephemeral 127.0.0.1 port.
async fn spawn_upstream() -> (Upstream, String) {
    let up = Upstream::default();
    let app = axum::Router::new()
        .fallback(upstream_handler)
        .with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (up, format!("http://{addr}"))
}

// ---------------------------------------------------------------------------
// Scarab: the public API and the broker over one state.
// ---------------------------------------------------------------------------

/// Stands in for TokenReview: `tok-<id>` is installation `<id>`'s projected
/// token.
struct FakeTokenReview;

#[async_trait::async_trait]
impl PluginTokenVerifier for FakeTokenReview {
    async fn verify(&self, token: &str) -> Result<VerifiedServiceAccount, TokenRejected> {
        let id = token
            .strip_prefix("tok-")
            .ok_or_else(|| TokenRejected("unknown token".into()))?;
        Ok(VerifiedServiceAccount {
            namespace: NAMESPACE.into(),
            service_account: format!("plugin-{id}"),
        })
    }
}

struct Harness {
    tdb: common::TestDb,
    pg: Arc<PostgresDb>,
    secrets: Arc<FakeSecrets>,
    public: axum::Router,
    broker: axum::Router,
    journal: Option<scarab_server::broker_proxy::BrokerJournalDrain>,
}

async fn harness() -> Option<Harness> {
    harness_with(|st| st).await
}

/// [`harness`], with the state adjusted before the routers are built.
async fn harness_with(adjust: impl FnOnce(AppState) -> AppState) -> Option<Harness> {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return None;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let secrets = Arc::new(FakeSecrets::new());
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let st = AppState::new(db, Arc::new(FakeClock::new(CLOCK_START_MS)), logs)
        .with_org_environments(pg.clone())
        .with_plugins(
            pg.clone(),
            PluginPolicy {
                enabled: true,
                namespace: NAMESPACE.into(),
                dev_allow_tag: false,
            },
        )
        .with_plugin_tokens(Arc::new(FakeTokenReview))
        .with_library_fetcher(Arc::new(argocd_fetcher()))
        .with_secrets(secrets.clone())
        // The fake upstream listens on 127.0.0.1.
        .with_broker_upstream_loopback_allowed(true);
    let st = adjust(st);
    let (broker, journal) = broker::router_with_journal(st.clone());
    Some(Harness {
        tdb,
        pg,
        secrets,
        public: router(st),
        broker,
        journal,
    })
}

impl Harness {
    async fn finish(self) {
        self.tdb.cleanup().await;
    }
}

/// A request through `app`, read to the end.
async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn admin(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let req = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let (status, _, body) = send(app, req).await;
    (status, body)
}

/// Install the Argo CD plugin as `argo` in `acme`, from its manifest's
/// source like any plugin; returns (id, token).
async fn install(h: &Harness) -> (String, String) {
    let (status, body) = admin(
        &h.public,
        "POST",
        "/v1/orgs/acme/plugins",
        Some(argocd_install_body(
            "argo",
            &format!("{REPO}@sha256:{}", "a".repeat(64)),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = format!("tok-{id}");
    (id, token)
}

async fn bind(h: &Harness, base_url: &str) -> (StatusCode, String) {
    admin(
        &h.public,
        "PUT",
        "/v1/orgs/acme/plugins/argo/connections/argocd",
        Some(json!({ "base_url": base_url, "credential": ARGO_TOKEN })),
    )
    .await
}

/// A plugin request to the broker, as `token`.
fn plugin_get(token: &str, path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/_scarab/plugin/v1/connections/argocd{path}"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// The journal is written off the request path: wait for `n` entries.
async fn journal(pg: &PostgresDb, installation_id: &str, n: usize) -> Vec<BrokerJournalEntry> {
    journal_until(pg, installation_id, |e| e.len() >= n).await
}

/// Wait until the installation's journal (newest first) satisfies `done`.
async fn journal_until(
    pg: &PostgresDb,
    installation_id: &str,
    done: impl Fn(&[BrokerJournalEntry]) -> bool,
) -> Vec<BrokerJournalEntry> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let entries = newest(pg, installation_id, 200).await;
        if done(&entries) {
            return entries;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the journal never got there: {entries:#?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The installation's newest `limit` journal entries, newest first.
async fn newest(pg: &PostgresDb, installation_id: &str, limit: usize) -> Vec<BrokerJournalEntry> {
    pg.broker_journal(installation_id, &BrokerJournalFilter::newest(limit))
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.entry)
        .collect()
}

/// The `phase` row of each request, newest first.
fn rows(entries: &[BrokerJournalEntry], phase: BrokerJournalPhase) -> Vec<&BrokerJournalEntry> {
    entries.iter().filter(|e| e.phase == phase).collect()
}

/// Read a response body until it ends; `true` if it ended in an error.
/// Panics if it is still open at `deadline`.
async fn ends_in_error(
    body: &mut (impl futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin),
    deadline: tokio::time::Instant,
) -> bool {
    loop {
        match tokio::time::timeout_at(deadline, body.next()).await {
            Err(_) => panic!("the stream was still open at the deadline"),
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) => return true,
            Ok(None) => return false,
        }
    }
}

/// Open the watch stream and read its first chunk.
async fn open_stream(
    h: &Harness,
    token: &str,
) -> impl futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin {
    let resp = h
        .broker
        .clone()
        .oneshot(plugin_get(token, "/api/v1/stream/applications"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(5), body.next())
        .await
        .expect("the stream delivers")
        .expect("a chunk")
        .expect("not an error");
    assert!(first.starts_with(b"{\"type\""));
    body
}

fn plugin_scope() -> SecretScope {
    SecretScope::Org {
        org: PLUGIN_CONNECTION_CREDENTIALS_ORG.into(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_bound_credential_is_injected_and_the_plugins_own_token_never_leaves() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (id, token) = install(&h).await;
    assert_eq!(bind(&h, &base).await.0, StatusCode::OK);

    let req = Request::builder()
        .method("GET")
        .uri(
            "/_scarab/plugin/v1/connections/argocd/api/v1/applications\
             ?projects=prod&selector=team%3Dweb+tier%3Dfront",
        )
        .header("authorization", format!("Bearer {token}"))
        .header("cookie", "argocd.token=from-the-plugin")
        .header("x-forwarded-host", "evil.example")
        // Headers an upstream may honour to change the method, the path or
        // the identity a read runs as: none may arrive.
        .header("x-http-method-override", "DELETE")
        .header("x-original-url", "/api/v1/session")
        .header("grpc-metadata-token", "another-identity")
        .header("impersonate-user", "admin")
        .header("accept", "application/json")
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = send(&h.broker, req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, r#"{"items":[{"metadata":{"name":"web"}}]}"#);
    assert!(
        headers.get("set-cookie").is_none(),
        "an upstream session never reaches the plugin"
    );

    let seen = up.seen();
    assert_eq!(seen.len(), 1);
    let s = &seen[0];
    // The query, re-encoded canonically.
    assert_eq!(
        s.path_and_query,
        "/api/v1/applications?projects=prod&selector=team%3Dweb%20tier%3Dfront"
    );
    assert_eq!(
        s.headers.get("authorization").unwrap(),
        &format!("Bearer {ARGO_TOKEN}"),
        "the bound credential replaces the plugin's"
    );
    assert_eq!(s.headers.get_all("authorization").iter().count(), 1);
    assert_eq!(s.headers.get("accept").unwrap(), "application/json");
    for (name, value) in &s.headers {
        let v = String::from_utf8_lossy(value.as_bytes());
        assert!(
            !v.contains(&token),
            "the plugin's ServiceAccount token reached the upstream in `{name}`"
        );
    }
    let names: Vec<&str> = s.headers.keys().map(|n| n.as_str()).collect();
    for name in [
        "cookie",
        "x-forwarded-host",
        "x-http-method-override",
        "x-original-url",
        "grpc-metadata-token",
        "impersonate-user",
    ] {
        assert!(!names.contains(&name), "`{name}` reached the upstream");
    }

    // Two rows: the head when the response started, the end when it ended.
    let entries = journal(&h.pg, &id, 2).await;
    let (head, end) = (
        rows(&entries, BrokerJournalPhase::Head),
        rows(&entries, BrokerJournalPhase::End),
    );
    assert_eq!((head.len(), end.len()), (1, 1), "{entries:#?}");
    assert_eq!(head[0].request_id, end[0].request_id);
    for row in [head[0], end[0]] {
        assert_eq!(row.status, Some(200));
        assert_eq!(row.rule.as_deref(), Some("GET /api/v1/applications"));
        assert_eq!(row.path, "/api/v1/applications");
        assert_eq!(row.upstream_path.as_deref(), Some("/api/v1/applications"));
        assert_eq!(
            row.query.as_deref(),
            Some("projects=prod&selector=team%3Dweb%20tier%3Dfront"),
            "the journal holds the query that was forwarded"
        );
        assert_eq!(row.denied_reason, None);
    }
    assert_eq!(head[0].bytes, None);
    assert_eq!(end[0].bytes, Some(body.len() as u64));
    h.finish().await;
}

#[tokio::test]
async fn a_query_key_the_rule_does_not_list_is_refused_journaled_and_never_sent() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (id, token) = install(&h).await;
    bind(&h, &base).await;

    let refused = [
        // Argo CD's hard refresh: a side effect behind a read path.
        "/api/v1/applications/web?refresh=hard",
        "/api/v1/applications/web?project=prod&refresh=normal",
        "/api/v1/applications/web?%72efresh=hard",
        // A rule without a query list allows no query.
        "/api/version?x=1",
        // Twice would be read either way.
        "/api/v1/applications/web?project=a&project=b",
    ];
    for path in refused {
        let (status, _, body) = send(&h.broker, plugin_get(&token, path)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
    }
    assert!(
        up.seen().is_empty(),
        "a refused request reached the upstream"
    );
    // What the rule lists goes through.
    let (status, _, _) = send(
        &h.broker,
        plugin_get(
            &token,
            "/api/v1/applications/web?appNamespace=argo&project=prod",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        up.seen()[0].path_and_query,
        "/api/v1/applications/web?appNamespace=argo&project=prod"
    );

    let entries = journal_until(&h.pg, &id, |e| {
        rows(e, BrokerJournalPhase::Refused).len() == refused.len()
    })
    .await;
    let refusals = rows(&entries, BrokerJournalPhase::Refused);
    assert!(refusals
        .iter()
        .all(|e| e.status == Some(403) && e.denied_reason.as_deref() == Some("query")));
    // The refused query is on record as sent.
    assert!(refusals
        .iter()
        .any(|e| e.query.as_deref() == Some("refresh=hard")));
    h.finish().await;
}

#[tokio::test]
async fn an_unmatched_or_traversing_path_is_refused_journaled_and_never_sent() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (id, token) = install(&h).await;
    bind(&h, &base).await;

    let refused = [
        "/api/v1/session",
        "/api/v1/applications/web/resource",
        "/api/v1/applications/..%2F..%2Fsession",
        "/api/v1/applications/%2e%2e",
        "/api/v1/applications/a%5Cb",
        "/api/v1/applications/%252e%252e",
        "/api//v1/applications",
        "/api/v1/applications/",
    ];
    for path in refused {
        let (status, _, body) = send(&h.broker, plugin_get(&token, path)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
    }
    assert!(
        up.seen().is_empty(),
        "a refused request reached the upstream"
    );

    let entries = journal(&h.pg, &id, refused.len()).await;
    assert!(entries.iter().all(|e| e.status == Some(403)
        && e.rule.is_none()
        && e.denied_reason.as_deref() == Some("no read rule")));
    assert!(entries.iter().any(|e| e.path == "/api/v1/session"));
    h.finish().await;
}

#[tokio::test]
async fn only_reads_without_a_body_are_forwarded() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (id, token) = install(&h).await;
    bind(&h, &base).await;

    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let req = Request::builder()
            .method(method)
            .uri("/_scarab/plugin/v1/connections/argocd/api/v1/applications")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = send(&h.broker, req).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(headers.get("allow").unwrap(), "GET, HEAD");
    }
    let with_body = Request::builder()
        .method("GET")
        .uri("/_scarab/plugin/v1/connections/argocd/api/v1/applications")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("content-length", "2")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(send(&h.broker, with_body).await.0, StatusCode::BAD_REQUEST);
    assert!(up.seen().is_empty());

    // HEAD rides on a GET rule.
    let head = Request::builder()
        .method("HEAD")
        .uri("/_scarab/plugin/v1/connections/argocd/api/version")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&h.broker, head).await.0, StatusCode::OK);
    assert_eq!(up.seen().len(), 1);

    let entries = journal(&h.pg, &id, 6).await;
    let reasons: Vec<Option<&str>> = entries.iter().map(|e| e.denied_reason.as_deref()).collect();
    assert_eq!(reasons.iter().filter(|r| **r == Some("method")).count(), 4);
    assert_eq!(
        reasons
            .iter()
            .filter(|r| **r == Some("request body"))
            .count(),
        1
    );
    h.finish().await;
}

#[tokio::test]
async fn the_destination_is_the_bound_base_url_whatever_the_request_says() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (_, token) = install(&h).await;
    bind(&h, &base).await;
    let bound_host = base.trim_start_matches("http://").to_string();

    // Absolute-form request target and a forged Host header.
    let req = Request::builder()
        .method("GET")
        .uri("http://evil.example:1/_scarab/plugin/v1/connections/argocd/api/version")
        .header("host", "evil.example:1")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = send(&h.broker, req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // A `*` segment that looks like an authority stays a path segment.
    let (status, _, _) = send(
        &h.broker,
        plugin_get(&token, "/api/v1/applications/@evil.example"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // As does an encoded `?`: it cannot start a query of the plugin's making.
    send(
        &h.broker,
        plugin_get(&token, "/api/v1/applications/x%3Fa=b"),
    )
    .await;

    let seen = up.seen();
    assert_eq!(seen.len(), 3);
    for s in &seen {
        assert_eq!(s.headers.get("host").unwrap(), bound_host.as_str());
    }
    assert_eq!(seen[0].path_and_query, "/api/version");
    assert_eq!(seen[1].path_and_query, "/api/v1/applications/@evil.example");
    assert_eq!(seen[2].path_and_query, "/api/v1/applications/x%3Fa=b");
    h.finish().await;
}

#[tokio::test]
async fn a_redirect_is_handed_back_never_followed() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (_, token) = install(&h).await;
    bind(&h, &base).await;

    let (status, headers, _) = send(
        &h.broker,
        plugin_get(&token, "/api/v1/applications/redirector"),
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    // Handed back, not followed — and without its `Location`, which can
    // name the bound upstream (response headers are allowlisted).
    assert!(headers.get("location").is_none());
    let paths: Vec<String> = up.seen().into_iter().map(|s| s.path_and_query).collect();
    assert_eq!(paths, vec!["/api/v1/applications/redirector"]);
    h.finish().await;
}

#[tokio::test]
async fn an_open_stream_is_cut_within_the_recheck_interval_of_a_kill() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (id, token) = install(&h).await;
    bind(&h, &base).await;

    let mut body = open_stream(&h, &token).await;
    // The stream is on record from its start, not only when it ends.
    let entries = journal(&h.pg, &id, 1).await;
    assert_eq!(entries[0].phase, BrokerJournalPhase::Head);

    let (status, _) = admin(&h.public, "POST", "/v1/orgs/acme/plugins/argo/kill", None).await;
    assert_eq!(status, StatusCode::OK);
    let deadline = tokio::time::Instant::now() + KILL_RECHECK_INTERVAL + Duration::from_secs(3);
    assert!(
        ends_in_error(&mut body, deadline).await,
        "a cut stream must end in an error, not a clean end"
    );

    // The upstream side is closed too, and the cut is journaled.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !up.stream_dropped.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the upstream stream stayed open"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let entries = journal_until(&h.pg, &id, |e| !rows(e, BrokerJournalPhase::End).is_empty()).await;
    let end = rows(&entries, BrokerJournalPhase::End)[0];
    assert_eq!(end.status, Some(200));
    assert!(
        end.denied_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("cut: the installation")),
        "{end:?}"
    );
    assert!(end.bytes.unwrap_or(0) > 0);

    // And the next request is refused at the door.
    let (status, _, _) = send(&h.broker, plugin_get(&token, "/api/version")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    h.finish().await;
}

/// The plugin stops reading, its buffer fills, the kill cuts the stream
/// while the broker cannot hand it even the error — and when it reads
/// again, what it buffered still ends in an error, never a clean end.
#[tokio::test]
async fn a_cut_stream_ends_in_an_error_even_for_a_plugin_that_stopped_reading() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (_, token) = install(&h).await;
    bind(&h, &base).await;

    let mut body = open_stream(&h, &token).await;
    let (status, _) = admin(&h.public, "POST", "/v1/orgs/acme/plugins/argo/kill", None).await;
    assert_eq!(status, StatusCode::OK);
    // Not reading: the upstream ticks every 100 ms, so the buffer is full
    // long before the re-check cuts, and the cut's error finds no room.
    let deadline = tokio::time::Instant::now() + KILL_RECHECK_INTERVAL + Duration::from_secs(3);
    while !up.stream_dropped.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the stream was not cut while the plugin was not reading"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        ends_in_error(
            &mut body,
            tokio::time::Instant::now() + Duration::from_secs(5)
        )
        .await,
        "a plugin that drains its buffer after a cut must still see an error"
    );
    h.finish().await;
}

#[tokio::test]
async fn a_rebind_or_an_unbind_cuts_an_open_stream() {
    let Some(h) = harness().await else {
        return;
    };
    let (_, base) = spawn_upstream().await;
    let (id, token) = install(&h).await;
    bind(&h, &base).await;

    let mut body = open_stream(&h, &token).await;
    assert_eq!(bind(&h, &base).await.0, StatusCode::OK);
    let deadline = tokio::time::Instant::now() + KILL_RECHECK_INTERVAL + Duration::from_secs(3);
    assert!(ends_in_error(&mut body, deadline).await, "re-bind");

    // On the new binding, a stream runs — until the slot is unbound.
    let mut body = open_stream(&h, &token).await;
    let (status, _) = admin(
        &h.public,
        "DELETE",
        "/v1/orgs/acme/plugins/argo/connections/argocd",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let deadline = tokio::time::Instant::now() + KILL_RECHECK_INTERVAL + Duration::from_secs(3);
    assert!(ends_in_error(&mut body, deadline).await, "unbind");

    let entries = journal_until(&h.pg, &id, |e| rows(e, BrokerJournalPhase::End).len() == 2).await;
    for end in rows(&entries, BrokerJournalPhase::End) {
        assert_eq!(
            end.denied_reason.as_deref(),
            Some("cut: the connection was re-bound or unbound"),
            "{end:?}"
        );
    }
    h.finish().await;
}

#[tokio::test]
async fn requests_past_the_installation_s_limit_are_429_before_any_read() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (id, token) = install(&h).await;
    bind(&h, &base).await;

    let mut open = Vec::new();
    for _ in 0..PROXY_CONCURRENCY_PER_INSTALLATION {
        open.push(open_stream(&h, &token).await);
    }
    let (status, headers, _) = send(&h.broker, plugin_get(&token, "/api/version")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers.get("retry-after").unwrap(), "1");
    assert_eq!(up.seen().len(), PROXY_CONCURRENCY_PER_INSTALLATION);
    let entries = journal_until(&h.pg, &id, |e| {
        !rows(e, BrokerJournalPhase::Refused).is_empty()
    })
    .await;
    let refused = rows(&entries, BrokerJournalPhase::Refused);
    assert_eq!(refused[0].status, Some(429));
    assert_eq!(refused[0].denied_reason.as_deref(), Some("concurrency"));
    assert_eq!(refused[0].path, "/api/version");

    // A permit comes back when its stream ends.
    drop(open.pop());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (status, _, _) = send(&h.broker, plugin_get(&token, "/api/version")).await;
        if status == StatusCode::OK {
            break;
        }
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(
            tokio::time::Instant::now() < deadline,
            "the permit never came back"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(open);
    h.finish().await;
}

/// With the journal writer stuck (its table locked) the queue fills, and
/// the broker refuses to forward rather than forward what it cannot journal
/// — and every request it did forward is on record once the writer moves.
#[tokio::test]
async fn a_full_journal_refuses_to_forward_and_nothing_forwarded_goes_unjournaled() {
    let Some(h) = harness_with(|st| st.with_broker_journal_queue(4)).await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (id, token) = install(&h).await;
    bind(&h, &base).await;

    let mut lock = h.tdb.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE plugin_broker_journal IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let mut forwarded = 0;
    let mut refused = false;
    for _ in 0..32 {
        let before = up.seen().len();
        let (status, headers, _) = send(&h.broker, plugin_get(&token, "/api/version")).await;
        if status == StatusCode::SERVICE_UNAVAILABLE {
            assert!(headers.get("retry-after").is_some());
            assert_eq!(up.seen().len(), before, "a 503 reached the upstream");
            refused = true;
            break;
        }
        assert_eq!(status, StatusCode::OK);
        forwarded += 1;
    }
    assert!(refused, "the journal queue never filled");
    assert_eq!(up.seen().len(), forwarded);
    lock.rollback().await.unwrap();

    let entries = journal_until(&h.pg, &id, |e| {
        rows(e, BrokerJournalPhase::Head).len() == forwarded
    })
    .await;
    assert_eq!(rows(&entries, BrokerJournalPhase::Head).len(), forwarded);
    h.finish().await;
}

#[tokio::test]
async fn an_unbound_slot_is_409_and_an_undeclared_one_404() {
    let Some(h) = harness().await else {
        return;
    };
    let (_, token) = install(&h).await;
    let (status, _, body) = send(&h.broker, plugin_get(&token, "/api/version")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let req = Request::builder()
        .method("GET")
        .uri("/_scarab/plugin/v1/connections/vault/v1/secret/data/prod")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&h.broker, req).await.0, StatusCode::NOT_FOUND);
    // Unauthenticated, nothing at all.
    let req = Request::builder()
        .method("GET")
        .uri("/_scarab/plugin/v1/connections/argocd/api/version")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&h.broker, req).await.0, StatusCode::UNAUTHORIZED);
    h.finish().await;
}

#[tokio::test]
async fn the_credential_is_never_readable_back_and_never_outlives_its_binding() {
    let Some(h) = harness().await else {
        return;
    };
    let (_, base) = spawn_upstream().await;
    let (_, token) = install(&h).await;

    let (status, put) = bind(&h, &base).await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let put: Value = serde_json::from_str(&put).unwrap();
    assert_eq!(put["configured"], json!(true));
    assert_eq!(put["credential_set"], json!(true));
    assert_eq!(put["insecure_http"], json!(true));
    assert_eq!(put["base_url"], json!(base));

    // Every read an admin or the plugin can make.
    let reads = [
        admin(
            &h.public,
            "GET",
            "/v1/orgs/acme/plugins/argo/connections",
            None,
        )
        .await,
        admin(&h.public, "GET", "/v1/orgs/acme/plugins/argo", None).await,
        admin(&h.public, "GET", "/v1/orgs/acme/plugins", None).await,
    ];
    for (status, body) in &reads {
        assert_eq!(*status, StatusCode::OK, "{body}");
        assert!(
            !body.contains(ARGO_TOKEN),
            "the credential was read back: {body}"
        );
    }
    // The credential scope is reserved: the generic secrets API does not
    // reach it at all (git-bug 97223c7).
    let (status, body) = admin(
        &h.public,
        "GET",
        &format!("/v1/secrets?org={PLUGIN_CONNECTION_CREDENTIALS_ORG}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(!body.contains(ARGO_TOKEN));
    let whoami = Request::builder()
        .uri("/_scarab/plugin/v1/whoami")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let (_, _, body) = send(&h.broker, whoami).await;
    assert!(!body.contains(ARGO_TOKEN));
    assert!(!put.to_string().contains(ARGO_TOKEN));

    // A re-bind mints a new key and deletes the old secret.
    let first: Vec<String> = h.secrets.list_scoped(&plugin_scope()).await.unwrap();
    assert_eq!(first.len(), 1);
    bind(&h, &base).await;
    let second = h.secrets.list_scoped(&plugin_scope()).await.unwrap();
    assert_eq!(second.len(), 1);
    assert_ne!(first, second);

    // Unbind and uninstall leave no credential behind.
    let (status, _) = admin(
        &h.public,
        "DELETE",
        "/v1/orgs/acme/plugins/argo/connections/argocd",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h
        .secrets
        .list_scoped(&plugin_scope())
        .await
        .unwrap()
        .is_empty());
    bind(&h, &base).await;
    let (status, _) = admin(&h.public, "DELETE", "/v1/orgs/acme/plugins/argo", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(h
        .secrets
        .list_scoped(&plugin_scope())
        .await
        .unwrap()
        .is_empty());
    h.finish().await;
}

#[tokio::test]
async fn a_binding_is_checked_before_anything_is_stored() {
    let Some(h) = harness().await else {
        return;
    };
    install(&h).await;
    let put = |slot: &str, body: Value| {
        let uri = format!("/v1/orgs/acme/plugins/argo/connections/{slot}");
        let public = h.public.clone();
        async move { admin(&public, "PUT", &uri, Some(body)).await }
    };
    for (slot, body) in [
        // not a slot the manifest declares
        (
            "vault",
            json!({ "base_url": "https://vault.test", "credential": "t" }),
        ),
        (
            "argocd",
            json!({ "base_url": "ftp://argo.test", "credential": "t" }),
        ),
        (
            "argocd",
            json!({ "base_url": "https://user:pw@argo.test", "credential": "t" }),
        ),
        (
            "argocd",
            json!({ "base_url": "https://argo.test/?x=1", "credential": "t" }),
        ),
        (
            "argocd",
            json!({ "base_url": "argo.test", "credential": "t" }),
        ),
        (
            "argocd",
            json!({ "base_url": "https://argo.test", "credential": "" }),
        ),
        (
            "argocd",
            json!({ "base_url": "https://argo.test", "credential": "two words" }),
        ),
        (
            "argocd",
            json!({ "base_url": "https://argo.test", "credential": "t\r\nX-Evil: 1" }),
        ),
        // a CA means nothing over http, and must be certificates
        (
            "argocd",
            json!({ "base_url": "http://argo.test", "credential": "t", "ca_pem": "x" }),
        ),
        (
            "argocd",
            json!({ "base_url": "https://argo.test", "credential": "t", "ca_pem": "not pem" }),
        ),
    ] {
        let (status, text) = put(slot, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {text}");
    }
    // A credential is required on every bind: no re-aiming a destination
    // with the credential entered for another.
    let (status, _) = put("argocd", json!({ "base_url": "https://argo.test" })).await;
    assert!(status.is_client_error());
    assert!(h
        .secrets
        .list_scoped(&plugin_scope())
        .await
        .unwrap()
        .is_empty());

    // A trailing slash is normalised away.
    let (status, body) = put(
        "argocd",
        json!({ "base_url": "https://argo.test/argo/", "credential": "t" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["base_url"], json!("https://argo.test/argo"));
    assert_eq!(body["insecure_http"], json!(false));
    h.finish().await;
}

/// ADR-0073 C5: with step-up ENFORCED on the state, the broker's read proxy
/// is exactly as before — it is its own listener and a PluginPrincipal never
/// meets the step-up guard. (The admin side — install, bind — is set up
/// with it off; it is not under test here.)
#[tokio::test]
async fn under_enforced_step_up_the_read_proxy_is_unchanged() {
    let stash: Arc<Mutex<Option<AppState>>> = Arc::default();
    let keep = stash.clone();
    let Some(h) = harness_with(move |st| {
        *keep.lock().unwrap() = Some(st.clone());
        st
    })
    .await
    else {
        return;
    };
    let on = stash.lock().unwrap().take().unwrap().with_step_up(
        scarab_server::step_up::StepUpService::new(
            Arc::new(scarab_testkit::InMemoryPasskeys::new()),
            Arc::new(scarab_testkit::InMemoryChallenges::new()),
            Arc::new(scarab_testkit::InMemoryStepUp::new()),
            Arc::new(scarab_testkit::InMemorySecurityEvents::new()),
            Arc::new(scarab_secrets_postgres::PostgresSecrets::with_keys(
                // The production sealer; a lazy pool never connects.
                sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://unused@localhost/unused")
                    .unwrap(),
                scarab_secrets_postgres::MasterKeySet::new(vec![[7u8; 32]]).unwrap(),
            )),
            scarab_webauthn::RelyingParty::from_public_url("https://ci.example.com", "Scarab")
                .unwrap(),
        ),
    );
    assert!(on.step_up.is_enforced());
    let enforced_broker = broker::router(on);

    let (up, base) = spawn_upstream().await;
    let (_, token) = install(&h).await;
    assert_eq!(bind(&h, &base).await.0, StatusCode::OK);
    let (status, _, body) =
        send(&enforced_broker, plugin_get(&token, "/api/v1/applications")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, r#"{"items":[{"metadata":{"name":"web"}}]}"#);
    let seen = up.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].headers.get("authorization").unwrap(),
        &format!("Bearer {ARGO_TOKEN}"),
        "the bound credential is injected, as without step-up"
    );
    h.finish().await;
}

/// The Postgres plugin store, with journal writes held until the test
/// lets them through: entries then pile up in the broker's queue.
struct HeldJournal {
    inner: Arc<dyn scarab_plugin::PluginStore>,
    open: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl scarab_plugin::PluginStore for HeldJournal {
    async fn install(
        &self,
        installation: &scarab_plugin::PluginInstallation,
    ) -> Result<scarab_plugin::PluginInstallation, scarab_plugin::PluginStoreError> {
        self.inner.install(installation).await
    }
    async fn installation(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Option<scarab_plugin::PluginInstallation>, scarab_plugin::PluginStoreError> {
        self.inner.installation(org, name).await
    }
    async fn installation_by_id(
        &self,
        id: &str,
    ) -> Result<Option<scarab_plugin::PluginInstallation>, scarab_plugin::PluginStoreError> {
        self.inner.installation_by_id(id).await
    }
    async fn installations(
        &self,
        org: &str,
    ) -> Result<Vec<scarab_plugin::PluginInstallation>, scarab_plugin::PluginStoreError> {
        self.inner.installations(org).await
    }
    async fn all_installations(
        &self,
    ) -> Result<Vec<scarab_plugin::PluginInstallation>, scarab_plugin::PluginStoreError> {
        self.inner.all_installations().await
    }
    async fn set_installation_state(
        &self,
        org: &str,
        name: &str,
        state: scarab_plugin::InstallationState,
        by: &str,
        at: i64,
    ) -> Result<Option<scarab_plugin::PluginInstallation>, scarab_plugin::PluginStoreError> {
        self.inner
            .set_installation_state(org, name, state, by, at)
            .await
    }
    async fn set_installation_storage(
        &self,
        org: &str,
        name: &str,
        storage: Option<&scarab_plugin::PluginStorage>,
        expected: Option<&scarab_plugin::PluginStorage>,
    ) -> Result<Option<scarab_plugin::PluginInstallation>, scarab_plugin::PluginStoreError> {
        self.inner
            .set_installation_storage(org, name, storage, expected)
            .await
    }
    async fn upgrade_installation(
        &self,
        org: &str,
        name: &str,
        expected_manifest_sha256: &str,
        upgrade: &scarab_plugin::PluginUpgrade,
    ) -> Result<Option<scarab_plugin::PluginUpgraded>, scarab_plugin::PluginStoreError> {
        self.inner
            .upgrade_installation(org, name, expected_manifest_sha256, upgrade)
            .await
    }
    async fn installation_versions(
        &self,
        installation_id: &str,
    ) -> Result<Vec<scarab_plugin::PluginInstallationVersion>, scarab_plugin::PluginStoreError>
    {
        self.inner.installation_versions(installation_id).await
    }
    async fn set_runtime_status(
        &self,
        id: &str,
        decided_in: scarab_plugin::InstallationState,
        decided_at_version: i64,
        status: scarab_plugin::RuntimeStatus,
        message: &str,
        probe_passed_at: Option<i64>,
    ) -> Result<bool, scarab_plugin::PluginStoreError> {
        self.inner
            .set_runtime_status(
                id,
                decided_in,
                decided_at_version,
                status,
                message,
                probe_passed_at,
            )
            .await
    }
    async fn uninstall(
        &self,
        org: &str,
        name: &str,
        by: &str,
        at: i64,
    ) -> Result<Option<scarab_plugin::PluginUninstall>, scarab_plugin::PluginStoreError> {
        self.inner.uninstall(org, name, by, at).await
    }
    async fn put_connection(
        &self,
        connection: &scarab_plugin::PluginConnection,
    ) -> Result<Option<String>, scarab_plugin::PluginStoreError> {
        self.inner.put_connection(connection).await
    }
    async fn connections(
        &self,
        installation_id: &str,
    ) -> Result<Vec<scarab_plugin::PluginConnection>, scarab_plugin::PluginStoreError> {
        self.inner.connections(installation_id).await
    }
    async fn connection(
        &self,
        installation_id: &str,
        slot: &str,
    ) -> Result<Option<scarab_plugin::PluginConnection>, scarab_plugin::PluginStoreError> {
        self.inner.connection(installation_id, slot).await
    }
    async fn delete_connection(
        &self,
        installation_id: &str,
        slot: &str,
    ) -> Result<Option<scarab_plugin::PluginConnection>, scarab_plugin::PluginStoreError> {
        self.inner.delete_connection(installation_id, slot).await
    }
    async fn append_broker_journal(
        &self,
        entries: &[BrokerJournalEntry],
    ) -> Result<(), scarab_plugin::PluginStoreError> {
        let _open = self.open.acquire().await.expect("never closed");
        self.inner.append_broker_journal(entries).await
    }
    async fn broker_journal(
        &self,
        installation_id: &str,
        filter: &BrokerJournalFilter,
    ) -> Result<Vec<scarab_plugin::BrokerJournalRecord>, scarab_plugin::PluginStoreError> {
        self.inner.broker_journal(installation_id, filter).await
    }
    async fn prune_broker_journal(
        &self,
        started_before: i64,
    ) -> Result<u64, scarab_plugin::PluginStoreError> {
        self.inner.prune_broker_journal(started_before).await
    }
}

/// git-bug 4711823 (1): at shutdown the journal writer writes every entry
/// queued so far before it stops, with the broker still serving. Without
/// the drain a rollout lost whatever the queue held. Journal writes are
/// held while the refusals arrive, so nearly all of them are still queued
/// when the drain starts.
#[tokio::test]
async fn shutdown_writes_the_journal_entries_still_queued() {
    let open = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = open.clone();
    let Some(mut h) = harness_with(move |st| {
        let held = HeldJournal {
            inner: st.plugins.clone().expect("plugins are on"),
            open: gate,
        };
        st.with_broker_plugin_store(Arc::new(held))
    })
    .await
    else {
        return;
    };
    let (id, token) = install(&h).await;
    const REFUSED: usize = 300;
    for _ in 0..REFUSED {
        let req = Request::builder()
            .method("GET")
            .uri("/_scarab/plugin/v1/connections/vault/v1/secret")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&h.broker, req).await.0, StatusCode::NOT_FOUND);
    }
    let drain = h.journal.take().expect("the read proxy is on");
    let finished = tokio::spawn(drain.finish(Duration::from_secs(10)));
    open.add_permits(1_000);
    assert!(finished.await.unwrap(), "the writer finished");
    // Read once, no waiting: everything queued before the drain is written.
    let written = newest(&h.pg, &id, REFUSED + 10).await;
    assert_eq!(written.len(), REFUSED);
    h.finish().await;
}

/// git-bug 4711823 (5): a `pc-*` credential no binding names is deleted
/// once it has stayed unreferenced for the grace period across sweeps; the
/// bound credential, a key just written (a bind in progress) and keys of
/// other shapes are kept.
#[tokio::test]
async fn an_orphaned_connection_credential_is_swept_after_its_grace() {
    use scarab_engine::Clock;
    use scarab_server::retention::{
        sweep_orphaned_connection_secrets, OrphanedConnectionSecrets,
        ORPHANED_CONNECTION_SECRET_GRACE_MS,
    };
    let Some(h) = harness().await else {
        return;
    };
    install(&h).await;
    assert_eq!(bind(&h, "https://argo.acme.test").await.0, StatusCode::OK);
    let bound = h.secrets.list_scoped(&plugin_scope()).await.unwrap();
    assert_eq!(bound.len(), 1, "{bound:?}");
    for key in ["pc-orphan", "not-a-connection-credential"] {
        h.secrets
            .put(
                &plugin_scope(),
                scarab_secrets::Secret {
                    key: key.into(),
                    value: b"x".to_vec(),
                },
            )
            .await
            .unwrap();
    }

    let db: Arc<dyn scarab_engine::Db> = Arc::new(InMemoryDb::new());
    let fake = Arc::new(FakeClock::new(CLOCK_START_MS));
    let clock: Arc<dyn Clock> = fake.clone();
    let mut seen = OrphanedConnectionSecrets::default();
    let n = sweep_orphaned_connection_secrets(
        &db,
        h.pg.as_ref(),
        h.secrets.as_ref(),
        &clock,
        "replica-a",
        &mut seen,
    )
    .await
    .unwrap();
    assert_eq!(n, 0, "first seen now: not yet");

    fake.advance(ORPHANED_CONNECTION_SECRET_GRACE_MS - 1);
    // A bind in progress: its secret is written, its binding not yet.
    h.secrets
        .put(
            &plugin_scope(),
            scarab_secrets::Secret {
                key: "pc-binding".into(),
                value: b"y".to_vec(),
            },
        )
        .await
        .unwrap();
    let n = sweep_orphaned_connection_secrets(
        &db,
        h.pg.as_ref(),
        h.secrets.as_ref(),
        &clock,
        "replica-a",
        &mut seen,
    )
    .await
    .unwrap();
    assert_eq!(n, 0, "one millisecond short of the grace");

    fake.advance(1);
    let n = sweep_orphaned_connection_secrets(
        &db,
        h.pg.as_ref(),
        h.secrets.as_ref(),
        &clock,
        "replica-a",
        &mut seen,
    )
    .await
    .unwrap();
    assert_eq!(n, 1, "the orphan, after its grace");
    let mut left = h.secrets.list_scoped(&plugin_scope()).await.unwrap();
    left.sort();
    let mut want = vec![
        bound[0].clone(),
        "not-a-connection-credential".to_string(),
        "pc-binding".to_string(),
    ];
    want.sort();
    assert_eq!(left, want);
    h.finish().await;
}

/// git-bug 4711823 (2): the broker reads a bound credential from the secret
/// store once and keeps it for its credential_ref; a re-bind mints a new
/// ref, so the new credential is read and sent from the next request on.
#[tokio::test]
async fn a_bound_credential_is_read_once_and_a_rebind_replaces_it() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (_, token) = install(&h).await;
    assert_eq!(bind(&h, &base).await.0, StatusCode::OK);
    let (status, _, body) = send(&h.broker, plugin_get(&token, "/api/version")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The secret store loses the credential: the cached one is still sent.
    let key = h.secrets.list_scoped(&plugin_scope()).await.unwrap()[0].clone();
    h.secrets.delete(&plugin_scope(), &key).await.unwrap();
    let (status, _, body) = send(&h.broker, plugin_get(&token, "/api/version")).await;
    assert_eq!(status, StatusCode::OK, "served from the cache: {body}");

    let (status, body) = admin(
        &h.public,
        "PUT",
        "/v1/orgs/acme/plugins/argo/connections/argocd",
        Some(json!({ "base_url": base, "credential": "argo-token-rotated" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = send(&h.broker, plugin_get(&token, "/api/version")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let sent: Vec<String> = up
        .seen()
        .iter()
        .map(|s| s.headers["authorization"].to_str().unwrap().to_string())
        .collect();
    assert_eq!(
        sent,
        [
            format!("Bearer {ARGO_TOKEN}"),
            format!("Bearer {ARGO_TOKEN}"),
            "Bearer argo-token-rotated".to_string(),
        ]
    );
    h.finish().await;
}

/// Move Tokio's clock (the broker's cache clock) forward by `by`, then let
/// it run in real time again from there.
async fn skip_ahead(by: Duration) {
    tokio::time::pause();
    tokio::time::advance(by).await;
    tokio::time::resume();
}

/// A secret rotated or deleted in the store itself, not through a re-bind,
/// stops being sent once the cached value is [`CREDENTIAL_MAX_AGE`] old: a
/// rotated one is read and sent, a deleted one refused (409, journaled
/// `credential missing`) and nothing reaches the upstream. Before that age
/// the cached value is sent, without reading the store again.
#[tokio::test]
async fn a_credential_changed_in_the_store_is_read_again_after_its_max_age() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (id, token) = install(&h).await;
    assert_eq!(bind(&h, &base).await.0, StatusCode::OK);
    let (status, _, body) = send(&h.broker, plugin_get(&token, "/api/version")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Rotated out of band: same key, new value.
    let key = h.secrets.list_scoped(&plugin_scope()).await.unwrap()[0].clone();
    h.secrets
        .put(
            &plugin_scope(),
            Secret {
                key: key.clone(),
                value: b"argo-token-rotated-in-the-store".to_vec(),
            },
        )
        .await
        .unwrap();
    skip_ahead(CREDENTIAL_MAX_AGE - Duration::from_secs(2)).await;
    let (status, _, body) = send(&h.broker, plugin_get(&token, "/api/version")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    skip_ahead(Duration::from_secs(3)).await;
    let (status, _, body) = send(&h.broker, plugin_get(&token, "/api/version")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        up.seen().last().unwrap().headers["authorization"],
        "Bearer argo-token-rotated-in-the-store",
        "past the max age the store is read again"
    );

    // Deleted out of band.
    h.secrets.delete(&plugin_scope(), &key).await.unwrap();
    let (status, _, body) = send(&h.broker, plugin_get(&token, "/api/version")).await;
    assert_eq!(status, StatusCode::OK, "still within the max age: {body}");
    skip_ahead(CREDENTIAL_MAX_AGE + Duration::from_secs(1)).await;
    let (status, _, body) = send(&h.broker, plugin_get(&token, "/api/version")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("has no credential"), "{body}");

    let sent: Vec<String> = up
        .seen()
        .iter()
        .map(|s| s.headers["authorization"].to_str().unwrap().to_string())
        .collect();
    assert_eq!(
        sent,
        [
            format!("Bearer {ARGO_TOKEN}"),
            // Within the max age: the cached value, the store not read.
            format!("Bearer {ARGO_TOKEN}"),
            "Bearer argo-token-rotated-in-the-store".to_string(),
            "Bearer argo-token-rotated-in-the-store".to_string(),
        ],
        "nothing reached the upstream after the deletion"
    );
    let entries = journal_until(&h.pg, &id, |e| e.iter().any(|e| e.status == Some(409))).await;
    let refused: Vec<_> = entries.iter().filter(|e| e.status == Some(409)).collect();
    assert_eq!(refused.len(), 1, "{entries:#?}");
    assert_eq!(
        refused[0].denied_reason.as_deref(),
        Some("credential missing")
    );
    h.finish().await;
}

/// git-bug 4711823 (4): a destination that is, or resolves only to, an
/// address the broker does not connect to is refused (502) and journaled,
/// and nothing reaches it. The fake upstream listens on 127.0.0.1, here
/// with loopback refused as in production: bound by IP literal (checked
/// before any connection) and by a name that resolves to it (checked in
/// the resolver, on the address the connection would use).
#[tokio::test]
async fn a_destination_on_loopback_or_link_local_is_refused_and_never_reached() {
    let Some(h) = harness_with(|st| st.with_broker_upstream_loopback_allowed(false)).await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (id, token) = install(&h).await;
    let port = base.rsplit(':').next().unwrap().trim_end_matches('/');
    for destination in [base.clone(), format!("http://localhost:{port}")] {
        assert_eq!(
            bind(&h, &destination).await.0,
            StatusCode::OK,
            "{destination}"
        );
        let (status, _, body) = send(&h.broker, plugin_get(&token, "/api/version")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{destination}: {body}");
        assert!(body.contains("does not connect to"), "{body}");
    }
    assert!(up.seen().is_empty(), "nothing reached the upstream");
    let refused = journal(&h.pg, &id, 2).await;
    assert!(
        refused
            .iter()
            .all(|e| e.denied_reason.as_deref() == Some("destination denied")),
        "{refused:?}"
    );
    h.finish().await;
}

/// git-bug 4711823 (3): a response body that holds the bound credential is
/// cut before any part of it reaches the Plugin, and the stream ends in an
/// error; the cut is journaled. A compressed body, which could not be read
/// for it, is refused; the Plugin's Accept-Encoding never goes upstream.
#[tokio::test]
async fn a_body_holding_the_credential_is_cut_and_a_compressed_one_refused() {
    let Some(h) = harness().await else {
        return;
    };
    let (up, base) = spawn_upstream().await;
    let (id, token) = install(&h).await;
    assert_eq!(bind(&h, &base).await.0, StatusCode::OK);

    let resp = h
        .broker
        .clone()
        .oneshot(plugin_get(&token, "/api/v1/applications?projects=echo"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body().into_data_stream();
    let mut delivered = Vec::new();
    let mut ended_in_error = false;
    while let Some(next) = tokio::time::timeout(Duration::from_secs(10), body.next())
        .await
        .expect("the stream ended")
    {
        match next {
            Ok(chunk) => delivered.extend_from_slice(&chunk),
            Err(_) => {
                ended_in_error = true;
                break;
            }
        }
    }
    assert!(ended_in_error, "a cut stream ends in an error");
    let delivered = String::from_utf8_lossy(&delivered).to_string();
    assert!(delivered.starts_with("{\"items\":[]}\n"), "{delivered}");
    let half = &ARGO_TOKEN[..ARGO_TOKEN.len() / 2];
    assert!(
        !delivered.contains(half),
        "no part of the credential: {delivered}"
    );
    let ends = journal_until(&h.pg, &id, |e| rows(e, BrokerJournalPhase::End).len() == 1).await;
    assert_eq!(
        rows(&ends, BrokerJournalPhase::End)[0]
            .denied_reason
            .as_deref(),
        Some("cut: the upstream response held the credential")
    );

    let req = Request::builder()
        .method("GET")
        .uri("/_scarab/plugin/v1/connections/argocd/api/v1/applications?projects=gzip")
        .header("authorization", format!("Bearer {token}"))
        .header("accept-encoding", "gzip")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = send(&h.broker, req).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("compressed"), "{body}");
    let last = up.seen().pop().unwrap();
    assert!(
        last.headers.get("accept-encoding").is_none(),
        "the Plugin's Accept-Encoding stays behind"
    );
    h.finish().await;
}

/// git-bug 4711823 (1, red team): a read forwarded before the drain, whose
/// upstream answers after the drain began, still gets its journal row. Its
/// slot was reserved before the queue closed; the credential has already
/// gone upstream.
#[tokio::test]
async fn shutdown_writes_the_row_of_a_read_still_waiting_on_its_upstream() {
    let Some(mut h) = harness().await else {
        return;
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut s, _) = listener.accept().await.unwrap();
        let _ = s.read(&mut [0u8; 4096]).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let _ = s
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
            .await;
    });
    let (id, token) = install(&h).await;
    assert_eq!(bind(&h, &base).await.0, StatusCode::OK);
    let broker = h.broker.clone();
    let read = tokio::spawn(async move { send(&broker, plugin_get(&token, "/api/version")).await });
    // The slot is reserved and the request is upstream; no row yet.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let drain = h.journal.take().expect("the read proxy is on");
    assert!(
        drain.finish(Duration::from_secs(10)).await,
        "the writer finished"
    );
    assert_eq!(read.await.unwrap().0, StatusCode::OK);
    let rows = newest(&h.pg, &id, 10).await;
    assert!(
        rows.iter().any(|e| e.phase == BrokerJournalPhase::Head),
        "the forwarded read has a head row: {rows:?}"
    );
    h.finish().await;
}
