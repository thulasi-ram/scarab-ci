//! The plugin endpoint (ADR-0086 §1, §2) and the OAuth authorization server
//! in front of it (§7), over HTTP through the real router.
//!
//! Real Postgres for every store (plugins, tokens, bindings, handles, OAuth
//! codes and grants). The fakes are the true externals: the login provider,
//! the client metadata document's host (`StaticFetcher`), the clock, and the
//! Plugin itself, a stand-in HTTP server on a local socket that the server
//! reaches through `with_plugin_endpoint_upstream` (no network). Each test
//! gets its own database and skips when `SCARAB_TEST_DATABASE_URL` is unset
//! (CI sets it).
//!
//! Not here: the passkey every consent needs, and the security event it
//! records. Both come from the step-up guard classing `oauth.consent` an
//! always-ask crown jewel, which `step_up_routes.rs` pins; this harness runs
//! with step-up off.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;

use common::{fresh_db, TestDb};
use scarab_db_postgres::PostgresDb;
use scarab_identity::{
    ApiTokenStore, Binding, BindingOrigin, OAuthClientMetadataHost, OAuthStore, Principal,
    PrincipalToken, PrincipalTokenKind, RbacStore, Role, Scope,
};
use scarab_plugin::{
    InstallationState, PluginInstallation, PluginManifest, PluginStore, RuntimeStatus,
};
use scarab_server::oauth_server::pkce_s256_challenge;
use scarab_server::plugin_endpoint::{
    canonical_endpoint_url, protected_resource_metadata_url, MAX_ENDPOINT_REQUEST_BYTES,
};
use scarab_server::plugins::PluginPolicy;
use scarab_server::test_support::StaticFetcher;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeAuthenticator, FakeClock, InMemoryObjectStore, InMemorySessions};

const CLOCK_START_MS: i64 = 1_000_000_000;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const PUBLIC: &str = "https://ci.example.com";
const NAMESPACE: &str = "scarab-plugins";
const PUBLISHER: &str = "ghcr.io/someone-else/mcp";
/// Distinctive, so a body that leaks it is caught.
const NAME: &str = "secret-mcp-name";
const ID: &str = "0000000001";
const CLIENT: &str = "https://claude.ai/oauth/mcp-client-metadata";
const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";
const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

// ---------------------------------------------------------------------------
// The stand-in Plugin
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Plugin {
    /// Each request the Plugin got: its headers and its body.
    seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    /// One message per `hold` request, once it arrived.
    held: mpsc::UnboundedSender<()>,
    /// `hold` requests answer once this turns true.
    release: watch::Receiver<bool>,
}

/// Answers by the body's `/params/mode`.
async fn plugin_answer(State(p): State<Plugin>, headers: HeaderMap, body: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    p.seen.lock().unwrap().push((headers, v.clone()));
    let mode = v
        .pointer("/params/mode")
        .and_then(Value::as_str)
        .unwrap_or("ok")
        .to_string();
    match mode.as_str() {
        "text" => (StatusCode::OK, [("content-type", "text/plain")], "hello").into_response(),
        "redirect" => (
            StatusCode::FOUND,
            [("location", "http://127.0.0.1:9/elsewhere")],
        )
            .into_response(),
        "error" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "boom" })),
        )
            .into_response(),
        "client" => (
            StatusCode::NOT_FOUND,
            Json(json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32601 } })),
        )
            .into_response(),
        "empty" => StatusCode::ACCEPTED.into_response(),
        "hold" => {
            let _ = p.held.send(());
            let mut release = p.release.clone();
            let _ = release.wait_for(|r| *r).await;
            Json(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })).into_response()
        }
        _ => (
            StatusCode::OK,
            [("x-evil", "1"), ("set-cookie", "stolen=1")],
            Json(json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// The world
// ---------------------------------------------------------------------------

/// A plugin with one endpoint `mcp`: `call` may read Runs and logs, `list`
/// and `reply` may use nothing.
fn manifest() -> PluginManifest {
    PluginManifest {
        plugin: "mcp".into(),
        image: None,
        capabilities: vec![],
        connections: vec![],
        config_keys: vec![],
        gates: Default::default(),
        extensions: vec![serde_json::from_value(json!({
            "point": "endpoint",
            "path": "mcp",
            "operations": {
                "call": {
                    "match": { "method": "POST", "json": { "/method": "tools/call" } },
                    "may": ["run.read", "step.logs.read"]
                },
                "list": {
                    "match": { "method": "POST", "json": { "/method": "tools/list" } },
                    "may": []
                },
                "reply": {
                    "match": { "method": "POST", "json": { "/result": { "exists": true } } },
                    "may": []
                }
            }
        }))
        .unwrap()],
    }
}

fn installation(id: &str, name: &str) -> PluginInstallation {
    PluginInstallation {
        id: id.into(),
        org: "acme".into(),
        name: name.into(),
        plugin: "mcp".into(),
        image: format!("{PUBLISHER}@sha256:{}", "a".repeat(64)),
        manifest: manifest(),
        manifest_sha256: "0".repeat(64),
        source: None,
        source_connection: None,
        config: BTreeMap::new(),
        state: InstallationState::Enabled,
        state_version: 0,
        runtime_status: RuntimeStatus::Pending,
        runtime_message: String::new(),
        probe_passed_at: None,
        approved_by: "ada".into(),
        approved_at: CLOCK_START_MS,
        killed_by: None,
        killed_at: None,
        storage: None,
    }
}

fn principal(subject: &str, roles: Vec<Role>) -> Principal {
    Principal {
        subject: subject.into(),
        handle: String::new(),
        display_name: None,
        roles,
    }
}

/// How the server is set up: which publishers it trusts, and whether the
/// OAuth server is on.
fn state(
    pg: &Arc<PostgresDb>,
    clock: &Arc<FakeClock>,
    fetcher: &Arc<StaticFetcher>,
    upstream: &str,
    publishers: &[&str],
    oauth_on: bool,
) -> AppState {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential("ada-code", principal("ada", vec![Role::Owner]))
            .with_credential("amy-code", principal("amy", vec![]))
            .with_credential("eve-code", principal("eve", vec![])),
    );
    let st = AppState::new(pg.clone(), clock.clone(), logs)
        .with_auth(auth, Arc::new(InMemorySessions::new()))
        .with_rbac(pg.clone())
        .with_api_tokens(pg.clone())
        .with_plugins(
            pg.clone(),
            PluginPolicy {
                enabled: true,
                namespace: NAMESPACE.into(),
                dev_allow_tag: false,
                trusted_publishers: publishers.iter().map(|p| p.to_string()).collect(),
            },
        )
        .with_delegated_calls(pg.clone())
        .with_public_url(PUBLIC)
        .with_plugin_endpoint_upstream(upstream)
        .with_library_fetcher(fetcher.clone());
    if oauth_on {
        st.with_oauth_server(pg.clone(), KEY.to_vec())
    } else {
        st
    }
}

/// A person signed in in a browser: their session cookie and CSRF token.
struct Browser {
    session: String,
    csrf: String,
}

struct World {
    tdb: TestDb,
    pg: Arc<PostgresDb>,
    clock: Arc<FakeClock>,
    fetcher: Arc<StaticFetcher>,
    upstream: String,
    app: Router,
    seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    held: mpsc::UnboundedReceiver<()>,
    release: watch::Sender<bool>,
    /// The global owner's session, as a Bearer.
    ada: String,
    amy: Browser,
}

impl World {
    async fn cleanup(self) {
        self.tdb.cleanup().await;
    }

    /// The same database served by another configuration.
    fn app_with(&self, publishers: &[&str], oauth_on: bool) -> Router {
        router(state(
            &self.pg,
            &self.clock,
            &self.fetcher,
            &self.upstream,
            publishers,
            oauth_on,
        ))
    }

    fn plugin_saw(&self) -> Vec<(HeaderMap, Value)> {
        self.seen.lock().unwrap().clone()
    }
}

async fn login(app: &Router, credential: &str) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "credential": credential }).to_string()))
        .unwrap();
    let (status, body) = read(send(app, req).await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

async fn browser(app: &Router, credential: &str) -> Browser {
    let v = login(app, credential).await;
    Browser {
        session: v["session"].as_str().unwrap().to_string(),
        csrf: v["csrf"].as_str().unwrap().to_string(),
    }
}

/// `mcp` installed in `acme`, amy a Member there, `claude.ai` an allowed
/// metadata host serving the client's document, and the stand-in Plugin up.
async fn world() -> Option<World> {
    let tdb = fresh_db().await?;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let clock = Arc::new(FakeClock::new(CLOCK_START_MS));

    let seen = Arc::new(Mutex::new(Vec::new()));
    let (held_tx, held) = mpsc::unbounded_channel();
    let (release, release_rx) = watch::channel(false);
    let plugin = Plugin {
        seen: seen.clone(),
        held: held_tx,
        release: release_rx,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let plugin_app = Router::new()
        .route("/mcp", axum::routing::post(plugin_answer))
        .with_state(plugin);
    tokio::spawn(async move {
        axum::serve(listener, plugin_app).await.unwrap();
    });

    let fetcher = Arc::new(StaticFetcher::new());
    fetcher.serve_url(
        CLIENT,
        &json!({ "client_id": CLIENT, "redirect_uris": [REDIRECT] }).to_string(),
    );
    let app = router(state(&pg, &clock, &fetcher, &upstream, &[PUBLISHER], true));

    PluginStore::install(&*pg, &installation(ID, NAME))
        .await
        .unwrap();
    RbacStore::grant(
        &*pg,
        &Binding {
            subject: "amy".into(),
            scope: Scope::Org("acme".into()),
            role: Role::Member,
        },
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    OAuthStore::put_client_metadata_host(
        &*pg,
        &OAuthClientMetadataHost {
            org: "acme".into(),
            host: "claude.ai".into(),
            added_by: "ada".into(),
            added_at: CLOCK_START_MS,
        },
    )
    .await
    .unwrap();
    let ada = login(&app, "ada-code").await["session"]
        .as_str()
        .unwrap()
        .to_string();
    let amy = browser(&app, "amy-code").await;
    Some(World {
        tdb,
        pg,
        clock,
        fetcher,
        upstream,
        app,
        seen,
        held,
        release,
        ada,
        amy,
    })
}

async fn send(app: &Router, req: Request<Body>) -> Response {
    app.clone().oneshot(req).await.unwrap()
}

async fn read(resp: Response) -> (StatusCode, String) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn resource(id: &str) -> String {
    canonical_endpoint_url(PUBLIC, "acme", id, "mcp")
}

/// An endpoint request for installation `id` with `headers` and `body`.
fn endpoint(id: &str, headers: &[(&str, &str)], body: impl Into<Body>) -> Request<Body> {
    let mut req = Request::builder().method("POST").uri(format!(
        "/v1/orgs/acme/plugin-installations/{id}/endpoints/mcp"
    ));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.body(body.into()).unwrap()
}

/// A JSON-RPC message the `list` operation matches, asking the stand-in
/// Plugin to answer in `mode`.
fn listing(mode: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": { "mode": mode } })
        .to_string()
}

/// `mcp`'s endpoint called with access token `token` and the `list` body.
async fn call_endpoint(app: &Router, token: &str, mode: &str) -> (StatusCode, String) {
    let bearer = format!("Bearer {token}");
    let req = endpoint(
        ID,
        &[
            ("authorization", bearer.as_str()),
            ("content-type", "application/json"),
        ],
        listing(mode),
    );
    read(send(app, req).await).await
}

/// amy's `oauth` access token `id` for `audience` in `org`, written
/// through the token store; its plaintext.
async fn put_oauth_token(pg: &PostgresDb, id: &str, org: &str, audience: &str) -> String {
    let plaintext = format!("scarab_oat_{id}");
    let token = PrincipalToken {
        id: id.into(),
        name: format!("claude.ai {id}"),
        owner_subject: "amy".into(),
        scope: Scope::Org(org.into()),
        role: Role::Member,
        expires_at: CLOCK_START_MS + DAY_MS,
        created_by: "amy".into(),
        created_at: CLOCK_START_MS,
        last_used_at: None,
        use_count: 0,
        revoked_at: None,
        kind: PrincipalTokenKind::Oauth,
        operations: Some(vec!["run.read".into()]),
        audience: Some(audience.into()),
        oauth_client: Some(CLIENT.into()),
    };
    ApiTokenStore::put(pg, &token, &token.stored_hash(&plaintext))
        .await
        .unwrap();
    plaintext
}

/// How many delegated caller handles are open.
async fn open_handles(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM delegated_caller_handles")
        .fetch_one(pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// The endpoint
// ---------------------------------------------------------------------------

/// The Plugin gets core's own re-serialisation of the body, a delegated
/// caller handle and a request id, and none of the caller's headers: not
/// their token, not their cookie. The caller gets the Plugin's JSON with
/// headers core sets and none of the Plugin's. The handle is deleted once
/// the call ends.
#[tokio::test]
async fn the_endpoint_forwards_with_a_handle_and_never_the_caller_s_credentials() {
    let Some(w) = world().await else {
        return;
    };
    let token = put_oauth_token(&w.pg, "t-amy", "acme", &resource(ID)).await;
    let bearer = format!("Bearer {token}");
    let cookie = format!("scarab_session={}", w.amy.session);
    let sent = json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": { "name": "get_run", "arguments": { "run_id": "r1" } }
    });
    let req = endpoint(
        ID,
        &[
            ("authorization", bearer.as_str()),
            ("cookie", cookie.as_str()),
            ("x-forwarded-for", "203.0.113.9"),
            ("content-type", "application/json; charset=utf-8"),
        ],
        sent.to_string(),
    );
    let resp = send(&w.app, req).await;
    let headers = resp.headers().clone();
    let (status, body) = read(resp).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } })
    );
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert!(headers.get("x-evil").is_none());
    assert!(headers.get("set-cookie").is_none());

    let saw = w.plugin_saw();
    assert_eq!(saw.len(), 1);
    let (plugin_headers, plugin_body) = &saw[0];
    assert_eq!(plugin_body, &sent);
    let handle = plugin_headers
        .get(scarab_plugin::PLUGIN_DELEGATED_CALLER_HANDLE_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(handle.starts_with("scarab_dch_"), "{handle:?}");
    assert_eq!(
        plugin_headers.get("x-request-id"),
        headers.get("x-scarab-endpoint-request-id")
    );
    for leaked in ["authorization", "cookie", "x-forwarded-for"] {
        assert!(
            plugin_headers.get(leaked).is_none(),
            "{leaked} reached the plugin"
        );
    }
    assert!(
        !format!("{plugin_headers:?}").contains(&token),
        "the caller's token reached the plugin"
    );

    // The handle goes when the call ends; its deletion is spawned, so let it
    // run (no sleep: yield until the row is gone).
    let mut left = open_handles(&w.tdb.pool).await;
    for _ in 0..1_000 {
        if left == 0 {
            break;
        }
        tokio::task::yield_now().await;
        left = open_handles(&w.tdb.pool).await;
    }
    assert_eq!(left, 0, "the handle outlived its call");
    w.cleanup().await;
}

/// Every refusal before the Plugin is reached: no usable token (a cookie
/// alone, an issued token, one for another installation's endpoint, another
/// org's, a revoked one), a body that is not JSON core takes (wrong type, an
/// encoding, too large, a repeated key, no operation, two operations), and
/// an installation that does not exist. The Plugin sees none of them.
#[tokio::test]
async fn the_endpoint_refuses_before_the_plugin_is_reached() {
    let Some(w) = world().await else {
        return;
    };
    let good = put_oauth_token(&w.pg, "t-good", "acme", &resource(ID)).await;
    let other = put_oauth_token(&w.pg, "t-other", "acme", &resource("0000000009")).await;
    let globex = put_oauth_token(&w.pg, "t-globex", "globex", &resource(ID)).await;
    let revoked = put_oauth_token(&w.pg, "t-revoked", "acme", &resource(ID)).await;
    ApiTokenStore::revoke(&*w.pg, "t-revoked", CLOCK_START_MS)
        .await
        .unwrap();
    let missing = put_oauth_token(&w.pg, "t-missing", "acme", &resource("0000000009")).await;
    let cookie = format!("scarab_session={}", w.amy.session);
    let bearer = |t: &str| format!("Bearer {t}");
    let (good_b, other_b, globex_b, revoked_b, missing_b) = (
        bearer(&good),
        bearer(&other),
        bearer(&globex),
        bearer(&revoked),
        bearer(&missing),
    );
    let json_ct = ("content-type", "application/json");
    let metadata = protected_resource_metadata_url(PUBLIC, "acme", ID, "mcp");

    let rows: Vec<(&str, Request<Body>, StatusCode)> = vec![
        (
            "a cookie alone",
            endpoint(ID, &[("cookie", cookie.as_str()), json_ct], listing("ok")),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "an issued token",
            endpoint(
                ID,
                &[("authorization", "Bearer scarab_pat_whatever"), json_ct],
                listing("ok"),
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "a token for another installation's endpoint",
            endpoint(
                ID,
                &[("authorization", other_b.as_str()), json_ct],
                listing("ok"),
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "a token of another org",
            endpoint(
                ID,
                &[("authorization", globex_b.as_str()), json_ct],
                listing("ok"),
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "a revoked token",
            endpoint(
                ID,
                &[("authorization", revoked_b.as_str()), json_ct],
                listing("ok"),
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "not JSON",
            endpoint(
                ID,
                &[
                    ("authorization", good_b.as_str()),
                    ("content-type", "text/plain"),
                ],
                listing("ok"),
            ),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "an encoding",
            endpoint(
                ID,
                &[
                    ("authorization", good_b.as_str()),
                    json_ct,
                    ("content-encoding", "gzip"),
                ],
                listing("ok"),
            ),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "too large",
            endpoint(
                ID,
                &[("authorization", good_b.as_str()), json_ct],
                format!(
                    "{{\"method\":\"tools/list\",\"pad\":\"{}\"}}",
                    "x".repeat(MAX_ENDPOINT_REQUEST_BYTES)
                ),
            ),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            "a repeated key",
            endpoint(
                ID,
                &[("authorization", good_b.as_str()), json_ct],
                r#"{"method":"tools/list","method":"tools/call"}"#,
            ),
            StatusCode::BAD_REQUEST,
        ),
        (
            "no operation",
            endpoint(
                ID,
                &[("authorization", good_b.as_str()), json_ct],
                r#"{"method":"resources/list"}"#,
            ),
            StatusCode::BAD_REQUEST,
        ),
        (
            "two operations",
            endpoint(
                ID,
                &[("authorization", good_b.as_str()), json_ct],
                r#"{"method":"tools/list","result":{}}"#,
            ),
            StatusCode::BAD_REQUEST,
        ),
        (
            "no such installation",
            endpoint(
                "0000000009",
                &[("authorization", missing_b.as_str()), json_ct],
                listing("ok"),
            ),
            StatusCode::NOT_FOUND,
        ),
    ];
    for (why, req, expected) in rows {
        let resp = send(&w.app, req).await;
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let (status, body) = read(resp).await;
        assert_eq!(status, expected, "{why}: {body}");
        let presented_a_bad_oauth_token = [
            "a token for another installation's endpoint",
            "a token of another org",
            "a revoked token",
        ]
        .contains(&why);
        if presented_a_bad_oauth_token {
            let c = challenge.unwrap_or_default();
            assert!(c.contains("invalid_token"), "{why}: {c}");
        }
        assert!(!body.contains(NAME), "{why} names the installation: {body}");
    }
    // A request with no token names where to get one, and says nothing more.
    let req = endpoint(ID, &[json_ct], listing("ok"));
    let resp = send(&w.app, req).await;
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        challenge,
        format!("Bearer resource_metadata=\"{metadata}\"")
    );

    assert!(
        w.plugin_saw().is_empty(),
        "a refused request reached the plugin"
    );
    assert_eq!(open_handles(&w.tdb.pool).await, 0);
    w.cleanup().await;
}

/// A killed, held or delisted installation answers 503 to its own callers
/// (it may run again) and a bare 401 to anyone without a token, with
/// nothing about it; with the OAuth server off the endpoint is a 404.
#[tokio::test]
async fn an_installation_that_cannot_run_answers_503_and_names_nothing_to_a_stranger() {
    let Some(w) = world().await else {
        return;
    };
    let token = put_oauth_token(&w.pg, "t-amy", "acme", &resource(ID)).await;

    PluginStore::set_installation_state(
        &*w.pg,
        "acme",
        NAME,
        InstallationState::Killed,
        "ada",
        CLOCK_START_MS,
    )
    .await
    .unwrap();
    let stranger = endpoint(ID, &[("content-type", "application/json")], listing("ok"));
    let (status, body) = read(send(&w.app, stranger).await).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!body.contains(NAME) && !body.contains("killed"), "{body}");
    let (status, body) = call_endpoint(&w.app, &token, "ok").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "killed: {body}");
    PluginStore::set_installation_state(
        &*w.pg,
        "acme",
        NAME,
        InstallationState::Enabled,
        "ada",
        CLOCK_START_MS,
    )
    .await
    .unwrap();

    let (status, body) = call_endpoint(&w.app_with(&[], true), &token, "ok").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "delisted: {body}");
    let (status, _) = call_endpoint(&w.app_with(&[PUBLISHER], false), &token, "ok").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "OAuth server off");

    let now = PluginStore::installation(&*w.pg, "acme", NAME)
        .await
        .unwrap()
        .unwrap();
    assert!(PluginStore::set_runtime_status(
        &*w.pg,
        ID,
        InstallationState::Enabled,
        now.state_version,
        RuntimeStatus::ProbeFailed,
        "network policy not enforced",
        None,
    )
    .await
    .unwrap());
    let (status, body) = call_endpoint(&w.app, &token, "ok").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "held: {body}");

    assert!(w.plugin_saw().is_empty());
    w.cleanup().await;
}

/// The Plugin's answer is passed on only as a 2xx or 4xx with a JSON body
/// or none; a non-JSON body, a redirect or a 5xx is a 502.
#[tokio::test]
async fn the_plugin_s_answer_is_passed_on_only_as_json_2xx_or_4xx() {
    let Some(w) = world().await else {
        return;
    };
    let token = put_oauth_token(&w.pg, "t-amy", "acme", &resource(ID)).await;
    for (mode, expected) in [
        ("text", StatusCode::BAD_GATEWAY),
        ("redirect", StatusCode::BAD_GATEWAY),
        ("error", StatusCode::BAD_GATEWAY),
        ("client", StatusCode::NOT_FOUND),
        ("empty", StatusCode::ACCEPTED),
    ] {
        let (status, body) = call_endpoint(&w.app, &token, mode).await;
        assert_eq!(status, expected, "{mode}: {body}");
        if expected == StatusCode::BAD_GATEWAY {
            assert!(!body.contains("boom") && !body.contains("hello"), "{body}");
        }
    }
    w.cleanup().await;
}

/// A caller has at most four requests open to one installation's endpoint:
/// the fifth is a 429 while they are open, and the limit frees as they end.
#[tokio::test]
async fn the_caller_s_concurrency_limit_holds_while_requests_are_open() {
    let Some(mut w) = world().await else {
        return;
    };
    let token = put_oauth_token(&w.pg, "t-amy", "acme", &resource(ID)).await;
    let mut open = Vec::new();
    for _ in 0..4 {
        let app = w.app.clone();
        let token = token.clone();
        open.push(tokio::spawn(async move {
            call_endpoint(&app, &token, "hold").await
        }));
    }
    for _ in 0..4 {
        w.held
            .recv()
            .await
            .expect("a held request reached the plugin");
    }
    let bearer = format!("Bearer {token}");
    let req = endpoint(
        ID,
        &[
            ("authorization", bearer.as_str()),
            ("content-type", "application/json"),
        ],
        listing("ok"),
    );
    let resp = send(&w.app, req).await;
    assert!(resp.headers().get("retry-after").is_some());
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    w.release.send(true).unwrap();
    for h in open {
        let (status, body) = h.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = call_endpoint(&w.app, &token, "ok").await;
    assert_eq!(status, StatusCode::OK, "after they end: {body}");
    w.cleanup().await;
}

// ---------------------------------------------------------------------------
// The OAuth authorization server
// ---------------------------------------------------------------------------

/// A verifier of PKCE's shape, distinct per `seed`.
fn verifier(seed: char) -> String {
    seed.to_string().repeat(48)
}

/// The authorization request for `mcp`'s endpoint, as the consent page
/// passes it on.
fn authorization(client_id: &str, redirect: &str, verifier: &str, method: &str) -> Value {
    json!({
        "response_type": "code",
        "client_id": client_id,
        "redirect_uri": redirect,
        "code_challenge": pkce_s256_challenge(verifier),
        "code_challenge_method": method,
        "resource": resource(ID),
        "state": "s1",
    })
}

/// POST `body` to `uri` as `who` in a browser.
async fn as_browser(app: &Router, who: &Browser, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("cookie", format!("scarab_session={}", who.session))
        .header("x-csrf-token", &who.csrf)
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, text) = read(send(app, req).await).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// `who`'s request with `bearer` instead of a browser session.
async fn as_bearer(
    app: &Router,
    method: &str,
    uri: &str,
    bearer: &str,
    body: Option<&Value>,
) -> StatusCode {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    send(app, req).await.status()
}

/// amy consents to `run.read` and `step.logs.read` with `verifier`: the
/// code. She signs in again first, since a test may have moved the clock
/// past her earlier session.
async fn consent(w: &World, verifier: &str) -> String {
    let mut body = authorization(CLIENT, REDIRECT, verifier, "S256");
    body["operations"] = json!(["run.read", "step.logs.read"]);
    let amy = browser(&w.app, "amy-code").await;
    let (status, answer) = as_browser(&w.app, &amy, "/v1/oauth/consent", &body).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let redirect = reqwest::Url::parse(answer["redirect_to"].as_str().unwrap()).unwrap();
    assert!(redirect.as_str().starts_with(REDIRECT), "{redirect}");
    let pairs: BTreeMap<String, String> = redirect.query_pairs().into_owned().collect();
    assert_eq!(pairs.get("state").map(String::as_str), Some("s1"));
    assert_eq!(pairs.get("iss").map(String::as_str), Some(PUBLIC));
    pairs["code"].clone()
}

/// `POST /oauth/token`, form-encoded.
async fn token_call(app: &Router, pairs: &[(&str, &str)]) -> (StatusCode, Value) {
    let form = reqwest::Url::parse_with_params("http://form.invalid/", pairs)
        .unwrap()
        .query()
        .unwrap_or_default()
        .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let (status, text) = read(send(app, req).await).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn exchange(
    app: &Router,
    code: &str,
    client: &str,
    redirect: &str,
    verifier: &str,
) -> (StatusCode, Value) {
    token_call(
        app,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client),
            ("redirect_uri", redirect),
            ("code_verifier", verifier),
        ],
    )
    .await
}

async fn refresh(app: &Router, secret: &str) -> (StatusCode, Value) {
    token_call(
        app,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", secret),
            ("client_id", CLIENT),
        ],
    )
    .await
}

/// A connected app: (access token, refresh secret).
async fn connect(w: &World, seed: char) -> (String, String) {
    let v = verifier(seed);
    let code = consent(w, &v).await;
    let (status, tokens) = exchange(&w.app, &code, CLIENT, REDIRECT, &v).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    (
        tokens["access_token"].as_str().unwrap().to_string(),
        tokens["refresh_token"].as_str().unwrap().to_string(),
    )
}

fn error_of(v: &Value) -> &str {
    v["error"].as_str().unwrap_or_default()
}

/// Both metadata documents say what a client needs to know: public clients
/// (`none`), client ID metadata documents, `S256` only; the endpoint's
/// document names its canonical URL and what it can be granted. With the
/// server off, neither exists.
#[tokio::test]
async fn the_metadata_documents_advertise_public_clients_and_metadata_documents() {
    let Some(w) = world().await else {
        return;
    };
    let get = |app: Router, uri: String| async move {
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let (status, text) = read(send(&app, req).await).await;
        (
            status,
            serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
        )
    };
    let (status, server) = get(
        w.app.clone(),
        "/.well-known/oauth-authorization-server".into(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(server["issuer"], json!(PUBLIC));
    assert_eq!(server["client_id_metadata_document_supported"], json!(true));
    assert_eq!(
        server["token_endpoint_auth_methods_supported"],
        json!(["none"])
    );
    assert_eq!(server["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(
        server["token_endpoint"],
        json!(format!("{PUBLIC}/oauth/token"))
    );

    let prm = protected_resource_metadata_url(PUBLIC, "acme", ID, "mcp")
        .trim_start_matches(PUBLIC)
        .to_string();
    let (status, endpoint_doc) = get(w.app.clone(), prm.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(endpoint_doc["resource"], json!(resource(ID)));
    assert_eq!(endpoint_doc["authorization_servers"], json!([PUBLIC]));
    assert_eq!(
        endpoint_doc["scopes_supported"],
        json!(["run.read", "step.logs.read"])
    );

    let off = w.app_with(&[PUBLISHER], false);
    let (status, _) = get(
        off.clone(),
        "/.well-known/oauth-authorization-server".into(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(off, prm).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    w.cleanup().await;
}

/// An authorization request is checked before anyone consents: a client
/// whose host the org does not allow, a document that names another client,
/// a redirect URI not listed exactly, PKCE other than `S256`. A person with
/// no role in the org gets the same 404 for an installation that exists and
/// one that does not, and no document is fetched for them. A token, issued
/// or `oauth`, never describes or consents.
#[tokio::test]
async fn an_authorization_request_is_checked_before_consent() {
    let Some(w) = world().await else {
        return;
    };
    let liar = "https://claude.ai/oauth/liar";
    w.fetcher.serve_url(
        liar,
        &json!({ "client_id": CLIENT, "redirect_uris": [REDIRECT] }).to_string(),
    );
    let v = verifier('v');
    let describe = "/v1/oauth/authorization-requests";
    for (why, body, expected) in [
        (
            "a host the org does not allow",
            authorization("https://other.example/oauth/client", REDIRECT, &v, "S256"),
            StatusCode::FORBIDDEN,
        ),
        (
            "a document naming another client",
            authorization(liar, REDIRECT, &v, "S256"),
            StatusCode::BAD_REQUEST,
        ),
        (
            "a redirect URI not listed exactly",
            authorization(CLIENT, &format!("{REDIRECT}/"), &v, "S256"),
            StatusCode::BAD_REQUEST,
        ),
        (
            "plain PKCE",
            authorization(CLIENT, REDIRECT, &v, "plain"),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let (status, answer) = as_browser(&w.app, &w.amy, describe, &body).await;
        assert_eq!(status, expected, "{why}: {answer}");
    }
    let (status, ok) = as_browser(
        &w.app,
        &w.amy,
        describe,
        &authorization(CLIENT, REDIRECT, &v, "S256"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ok}");
    assert_eq!(ok["client_host"], json!("claude.ai"));
    assert_eq!(ok["installation_id"], json!(ID));
    let offered: Vec<&str> = ok["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(offered, vec!["run.read", "step.logs.read"]);

    let eve = browser(&w.app, "eve-code").await;
    let fetched_before = w.fetcher.asked().len();
    let mut missing = authorization(CLIENT, REDIRECT, &v, "S256");
    missing["resource"] = json!(resource("0000000009"));
    let (existing_status, existing_body) = as_browser(
        &w.app,
        &eve,
        describe,
        &authorization(CLIENT, REDIRECT, &v, "S256"),
    )
    .await;
    let (missing_status, missing_body) = as_browser(&w.app, &eve, describe, &missing).await;
    assert_eq!(existing_status, StatusCode::NOT_FOUND, "{existing_body}");
    assert_eq!(missing_status, StatusCode::NOT_FOUND, "{missing_body}");
    assert_eq!(existing_body, missing_body);
    assert_eq!(
        w.fetcher.asked().len(),
        fetched_before,
        "a document was fetched for eve"
    );

    let issued = format!("{}amys-cli-token", scarab_identity::API_TOKEN_PREFIX);
    let record = PrincipalToken {
        id: "t-issued".into(),
        name: "cli".into(),
        owner_subject: "amy".into(),
        scope: Scope::Org("acme".into()),
        role: Role::Member,
        expires_at: CLOCK_START_MS + DAY_MS,
        created_by: "amy".into(),
        created_at: CLOCK_START_MS,
        last_used_at: None,
        use_count: 0,
        revoked_at: None,
        kind: PrincipalTokenKind::Issued,
        operations: None,
        audience: None,
        oauth_client: None,
    };
    ApiTokenStore::put(&*w.pg, &record, &record.stored_hash(&issued))
        .await
        .unwrap();
    let (access, _) = connect(&w, 'c').await;
    let mut consent_body = authorization(CLIENT, REDIRECT, &v, "S256");
    consent_body["operations"] = json!(["run.read"]);
    for (why, bearer) in [
        ("an issued token", issued.as_str()),
        ("an oauth token", access.as_str()),
    ] {
        for uri in [describe, "/v1/oauth/consent"] {
            let status = as_bearer(&w.app, "POST", uri, bearer, Some(&consent_body)).await;
            assert!(
                status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED,
                "{why} at {uri}: {status}"
            );
        }
    }
    w.cleanup().await;
}

/// A code is spent once, and only by its client, redirect URI and verifier:
/// wrong ones spend nothing. Its token works at the endpoint. Presenting the
/// spent code again with the right verifier revokes the grant it became,
/// and its token stops working. A code past its two minutes is refused.
#[tokio::test]
async fn a_code_is_spent_once_by_its_verifier_and_a_replay_revokes_its_grant() {
    let Some(w) = world().await else {
        return;
    };
    let v = verifier('a');
    let code = consent(&w, &v).await;
    for (why, client, redirect, verifier) in [
        ("another verifier", CLIENT, REDIRECT, "b".repeat(48)),
        (
            "another client",
            "https://claude.ai/oauth/other",
            REDIRECT,
            v.clone(),
        ),
        (
            "another redirect URI",
            CLIENT,
            "https://claude.ai/other",
            v.clone(),
        ),
    ] {
        let (status, answer) = exchange(&w.app, &code, client, redirect, &verifier).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {answer}");
        assert_eq!(error_of(&answer), "invalid_grant", "{why}");
    }
    let (status, tokens) = exchange(&w.app, &code, CLIENT, REDIRECT, &v).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the wrong guesses spent nothing: {tokens}"
    );
    assert_eq!(tokens["token_type"], json!("Bearer"));
    assert_eq!(tokens["scope"], json!("run.read step.logs.read"));
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let secret = tokens["refresh_token"].as_str().unwrap().to_string();
    let (status, body) = call_endpoint(&w.app, &access, "ok").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, answer) = exchange(&w.app, &code, CLIENT, REDIRECT, &v).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_of(&answer), "invalid_grant");
    let (status, _) = call_endpoint(&w.app, &access, "ok").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the replay revoked the grant"
    );
    let (status, answer) = refresh(&w.app, &secret).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_of(&answer), "invalid_grant");

    let late = verifier('l');
    let code = consent(&w, &late).await;
    w.clock.advance(2 * 60 * 1000);
    let (status, answer) = exchange(&w.app, &code, CLIENT, REDIRECT, &late).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(error_of(&answer), "invalid_grant");
    w.cleanup().await;
}

/// A refresh rotates the secret and replaces the access token. The secret
/// before may be retried once inside 30 s (a lost response) and gets the
/// same successor; a second retry is refused without revoking. Used again
/// after the window, it revokes the grant and every access token of it.
#[tokio::test]
async fn refresh_rotates_allows_one_retry_and_revokes_on_reuse() {
    let Some(w) = world().await else {
        return;
    };
    let (a0, r0) = connect(&w, 'a').await;
    let (status, first) = refresh(&w.app, &r0).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let a1 = first["access_token"].as_str().unwrap().to_string();
    let r1 = first["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(r1, r0);
    assert_eq!(
        call_endpoint(&w.app, &a0, "ok").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(call_endpoint(&w.app, &a1, "ok").await.0, StatusCode::OK);

    w.clock.advance(10_000);
    let (status, retried) = refresh(&w.app, &r0).await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["refresh_token"], json!(r1), "the same successor");
    let a2 = retried["access_token"].as_str().unwrap().to_string();
    assert_ne!(a2, a1);
    assert_eq!(call_endpoint(&w.app, &a2, "ok").await.0, StatusCode::OK);

    let (status, again) = refresh(&w.app, &r0).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{again}");
    assert_eq!(error_of(&again), "invalid_grant");
    assert_eq!(
        call_endpoint(&w.app, &a2, "ok").await.0,
        StatusCode::OK,
        "a second retry revokes nothing"
    );

    w.clock.advance(31_000);
    let (status, reused) = refresh(&w.app, &r0).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reused}");
    assert_eq!(error_of(&reused), "invalid_grant");
    for (which, token) in [("a1", &a1), ("a2", &a2)] {
        assert_eq!(
            call_endpoint(&w.app, token, "ok").await.0,
            StatusCode::UNAUTHORIZED,
            "{which} outlived the grant"
        );
    }
    let (status, _) = refresh(&w.app, &r1).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the current secret died with the grant"
    );
    w.cleanup().await;
}

/// What ends a grant and what does not: a kill answers 503 and keeps it
/// (enable, and it refreshes); removing the client's metadata host, the
/// person disconnecting the app (from a browser, never with a token), 30
/// days unused, 90 days in all, and an uninstall each end it.
#[tokio::test]
async fn kill_keeps_a_grant_and_a_removed_host_disconnect_age_or_uninstall_ends_it() {
    let Some(w) = world().await else {
        return;
    };
    let host_uri = "/v1/orgs/acme/oauth/client-metadata-hosts/claude.ai";

    // Kill: 503, the grant stays.
    let (_, secret) = connect(&w, 'k').await;
    for state in [InstallationState::Killed, InstallationState::Enabled] {
        PluginStore::set_installation_state(&*w.pg, "acme", NAME, state, "ada", CLOCK_START_MS)
            .await
            .unwrap();
        if state == InstallationState::Killed {
            let (status, answer) = refresh(&w.app, &secret).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{answer}");
            assert_eq!(error_of(&answer), "temporarily_unavailable");
        }
    }
    let (status, answer) = refresh(&w.app, &secret).await;
    assert_eq!(status, StatusCode::OK, "after enable: {answer}");

    // A removed metadata host ends its clients' grants.
    let (access, secret) = connect(&w, 'h').await;
    assert_eq!(
        as_bearer(&w.app, "DELETE", host_uri, &w.ada, None).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call_endpoint(&w.app, &access, "ok").await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, answer) = refresh(&w.app, &secret).await;
    assert_eq!(
        (status, error_of(&answer)),
        (StatusCode::BAD_REQUEST, "invalid_grant")
    );
    assert_eq!(
        as_bearer(&w.app, "PUT", host_uri, &w.ada, None).await,
        StatusCode::NO_CONTENT
    );

    // Disconnect: never with a token; from the browser it ends the grant.
    let (access, secret) = connect(&w, 'd').await;
    let req = Request::builder()
        .uri("/v1/me/connected-apps")
        .header("cookie", format!("scarab_session={}", w.amy.session))
        .body(Body::empty())
        .unwrap();
    let (status, apps) = read(send(&w.app, req).await).await;
    assert_eq!(status, StatusCode::OK, "{apps}");
    let apps: Value = serde_json::from_str(&apps).unwrap();
    let id = apps
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["revoked_at"].is_null())
        .and_then(|a| a["id"].as_str())
        .unwrap()
        .to_string();
    let disconnect = format!("/v1/me/connected-apps/{id}");
    assert_ne!(
        as_bearer(&w.app, "DELETE", &disconnect, &access, None).await,
        StatusCode::NO_CONTENT,
        "a token disconnected an app"
    );
    let req = Request::builder()
        .method("DELETE")
        .uri(&disconnect)
        .header("cookie", format!("scarab_session={}", w.amy.session))
        .header("x-csrf-token", &w.amy.csrf)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&w.app, req).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        call_endpoint(&w.app, &access, "ok").await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _) = refresh(&w.app, &secret).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 90 days in all, even when used every 29.
    let (_, mut secret) = connect(&w, 'n').await;
    for _ in 0..3 {
        w.clock.advance(29 * DAY_MS);
        let (status, answer) = refresh(&w.app, &secret).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        secret = answer["refresh_token"].as_str().unwrap().to_string();
    }
    w.clock.advance(4 * DAY_MS);
    let (status, answer) = refresh(&w.app, &secret).await;
    assert_eq!(
        (status, error_of(&answer)),
        (StatusCode::BAD_REQUEST, "invalid_grant")
    );

    // 30 days unused.
    let (_, secret) = connect(&w, 'i').await;
    w.clock.advance(30 * DAY_MS);
    let (status, answer) = refresh(&w.app, &secret).await;
    assert_eq!(
        (status, error_of(&answer)),
        (StatusCode::BAD_REQUEST, "invalid_grant")
    );

    // Uninstall: its tokens go at once, and refresh is `invalid_grant`.
    let (access, secret) = connect(&w, 'u').await;
    let ada = login(&w.app, "ada-code").await["session"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        as_bearer(
            &w.app,
            "DELETE",
            &format!("/v1/orgs/acme/plugins/{NAME}"),
            &ada,
            None
        )
        .await,
        StatusCode::OK
    );
    let revoked: Option<i64> = sqlx::query_scalar(
        "SELECT revoked_at FROM api_tokens WHERE kind = 'oauth' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&w.tdb.pool)
    .await
    .unwrap();
    assert!(revoked.is_some(), "the uninstall left {access} live");
    let (status, answer) = refresh(&w.app, &secret).await;
    assert_eq!(
        (status, error_of(&answer)),
        (StatusCode::BAD_REQUEST, "invalid_grant")
    );
    w.cleanup().await;
}

/// A store failure on the token endpoint is a 503 `temporarily_unavailable`
/// with no detail of what failed.
#[tokio::test]
async fn a_store_failure_on_the_token_endpoint_is_503_without_detail() {
    let Some(w) = world().await else {
        return;
    };
    let (_, secret) = connect(&w, 's').await;
    sqlx::raw_sql("ALTER TABLE oauth_grants RENAME TO oauth_grants_away")
        .execute(&w.tdb.pool)
        .await
        .unwrap();
    let (status, answer) = refresh(&w.app, &secret).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{answer}");
    assert_eq!(error_of(&answer), "temporarily_unavailable");
    let text = answer.to_string();
    for detail in ["oauth_grants", "relation", "postgres", "sqlx"] {
        assert!(!text.contains(detail), "{text}");
    }
    sqlx::raw_sql("ALTER TABLE oauth_grants_away RENAME TO oauth_grants")
        .execute(&w.tdb.pool)
        .await
        .unwrap();
    w.cleanup().await;
}
