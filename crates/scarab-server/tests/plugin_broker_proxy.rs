//! The CredentialBroker's read proxy over HTTP (ADR-0072 §5, MMP slice d3):
//! an org admin binds the first-party plugin's `argocd` connection slot on
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
    BrokerJournalEntry, BrokerJournalPhase, PluginStore, PluginTokenVerifier, TokenRejected,
    VerifiedServiceAccount,
};
use scarab_secrets::{SecretProvider, SecretScope};
use scarab_server::broker_proxy::{KILL_RECHECK_INTERVAL, PROXY_CONCURRENCY_PER_INSTALLATION};
use scarab_server::plugins::{PluginPolicy, PLUGIN_CONNECTION_CREDENTIALS_ORG};
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
        .with_deployments(pg.clone())
        .with_org_environments(pg.clone())
        .with_plugins(
            pg.clone(),
            PluginPolicy {
                enabled: true,
                namespace: NAMESPACE.into(),
                argocd_repo: REPO.into(),
                dev_allow_tag: false,
            },
        )
        .with_plugin_tokens(Arc::new(FakeTokenReview))
        .with_secrets(secrets.clone());
    let st = adjust(st);
    Some(Harness {
        tdb,
        pg,
        secrets,
        public: router(st.clone()),
        broker: broker::router(st),
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

/// Install the first-party plugin as `argo` in `acme`; returns (id, token).
async fn install(h: &Harness) -> (String, String) {
    let (status, body) = admin(
        &h.public,
        "POST",
        "/v1/orgs/acme/plugins",
        Some(json!({
            "name": "argo",
            "plugin": "argocd",
            "image": format!("{REPO}@sha256:{}", "a".repeat(64)),
        })),
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
        let entries = pg.broker_journal(installation_id, 200).await.unwrap();
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
        admin(
            &h.public,
            "GET",
            &format!("/v1/secrets?org={PLUGIN_CONNECTION_CREDENTIALS_ORG}"),
            None,
        )
        .await,
    ];
    for (status, body) in &reads {
        assert_eq!(*status, StatusCode::OK, "{body}");
        assert!(
            !body.contains(ARGO_TOKEN),
            "the credential was read back: {body}"
        );
    }
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
