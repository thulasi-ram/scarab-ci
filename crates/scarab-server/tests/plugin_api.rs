//! PluginApi over HTTP (ADR-0086 §3, §4, §11): a `trusted_service` Plugin
//! calls the public API's own routes on the broker listener for one caller,
//! with its installation token and a DelegatedCallerHandle.
//!
//! What must hold: the call runs as the caller (her live role and scope,
//! never the installation's), every write's trail names the installation,
//! every refusal happens before the handler, a refusal after the handle is
//! known is journaled with its status, a call that cannot be journaled is not
//! made and spends nothing, and kill, uninstall, a new digest, a narrowing
//! upgrade and a delisted publisher each end the handles already open.
//!
//! Real Postgres for every store (plugins, tokens, bindings, handles, the
//! journal, runs). The fakes are the TokenReview verifier and the manifest
//! source (true externals) and the clock. The caller's `oauth` tokens are
//! written through the token store, as nothing mints one yet; a few handles
//! are written through the handle store directly, so the middleware is
//! tested without trusting the mint. Each test gets its own database and
//! skips when `SCARAB_TEST_DATABASE_URL` is unset (CI sets it).

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{fresh_db, TestDb};
use scarab_db_postgres::PostgresDb;
use scarab_engine::{Db, EventPayload, RunId};
use scarab_identity::{
    ApiTokenStore, Binding, BindingOrigin, PrincipalToken, PrincipalTokenKind, RbacStore, Role,
    Scope,
};
use scarab_plugin::{
    DelegatedCallStore, DelegatedCallerHandle, PluginInstallation, PluginStore,
    PluginTokenVerifier, TokenRejected, VerifiedServiceAccount,
};
use scarab_server::plugin_api::{
    delegated_caller_handle_hash, mint_delegated_caller_handle, DELEGATED_CALLER_HANDLE_HEADER,
};
use scarab_server::plugins::PluginPolicy;
use scarab_server::test_support::StaticFetcher;
use scarab_server::{broker, router, ApiError, AppState, LogService};
use scarab_testkit::{FakeClock, FakeForge, InMemoryObjectStore};

const CLOCK_START_MS: i64 = 1_000_000;
const HOUR_MS: i64 = 60 * 60 * 1000;
const NAMESPACE: &str = "scarab-plugins";
const PUBLISHER: &str = "ghcr.io/someone-else/mcp";

/// Stands in for TokenReview: `tok-<id>` is installation `<id>`'s token.
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

/// The plugin's image, by digest, its digest all `c`.
fn image(c: char) -> String {
    format!("{PUBLISHER}@sha256:{}", c.to_string().repeat(64))
}

/// The manifest's file at commit `c` (40 of it).
fn source(c: char) -> String {
    format!(
        "github.com/someone-else/scarab-mcp//manifest.json@{}",
        c.to_string().repeat(40)
    )
}

/// A plugin with one endpoint `mcp`: `tools_list` may use nothing,
/// `read_run` may read Runs, `cancel_run` may use `cancel_may` with one
/// write.
fn endpoint_manifest(cancel_may: &[&str]) -> String {
    json!({
        "plugin": "mcp",
        "capabilities": ["use_connection"],
        "connections": [{
            "name": "slack",
            "auth_schemes": ["bearer"],
            "read_rules": [{ "method": "GET", "path": "/api/conversations.info", "query": ["channel"] }]
        }],
        "extensions": [{
            "point": "endpoint",
            "path": "mcp",
            "operations": {
                "tools_list": {
                    "match": { "method": "POST", "json": { "/method": "tools/list" } },
                    "may": []
                },
                "read_run": {
                    "match": { "method": "POST", "json": { "/params/name": "read_run" } },
                    "may": ["run.read"]
                },
                "cancel_run": {
                    "match": { "method": "POST", "json": { "/params/name": "cancel_run" } },
                    "may": cancel_may,
                    "writes": 1
                }
            }
        }]
    })
    .to_string()
}

/// The same plugin with no endpoint: it may not call PluginApi.
fn plain_manifest() -> String {
    let mut m: Value = serde_json::from_str(&endpoint_manifest(&["run.cancel"])).unwrap();
    m.as_object_mut().unwrap().remove("extensions");
    m["plugin"] = json!("plain");
    m.to_string()
}

fn plain_source() -> String {
    format!(
        "github.com/someone-else/scarab-plain//manifest.json@{}",
        "a".repeat(40)
    )
}

fn state(
    pg: &Arc<PostgresDb>,
    clock: &Arc<FakeClock>,
    fetcher: &Arc<StaticFetcher>,
    publishers: &[&str],
) -> AppState {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    AppState::new(pg.clone(), clock.clone(), logs)
        .with_plugins(
            pg.clone(),
            PluginPolicy {
                enabled: true,
                namespace: NAMESPACE.into(),
                dev_allow_tag: false,
                trusted_publishers: publishers.iter().map(|p| p.to_string()).collect(),
            },
        )
        .with_plugin_tokens(Arc::new(FakeTokenReview))
        .with_plugin_api_tokens(Arc::new(FakeTokenReview))
        .with_api_tokens(pg.clone())
        .with_rbac(pg.clone())
        .with_delegated_calls(pg.clone())
        .with_forge(Arc::new(FakeForge::new()))
        .with_library_fetcher(fetcher.clone())
}

struct World {
    tdb: TestDb,
    pg: Arc<PostgresDb>,
    clock: Arc<FakeClock>,
    fetcher: Arc<StaticFetcher>,
    st: AppState,
    /// The public API, authz off (every caller an owner): seeds Runs and
    /// administers the plugin.
    public: axum::Router,
    /// The broker listener: PluginApi.
    broker: axum::Router,
    /// The endpoint plugin `mcp` in `acme`.
    mcp: PluginInstallation,
}

impl World {
    /// The installation as stored now.
    async fn installation(&self, name: &str) -> PluginInstallation {
        PluginStore::installation(&*self.pg, "acme", name)
            .await
            .unwrap()
            .unwrap()
    }

    async fn cleanup(self) {
        self.tdb.cleanup().await;
    }
}

/// A fresh database; `mcp` installed in `acme` from a listed publisher; amy
/// a Member of `acme`.
async fn world() -> Option<World> {
    let tdb = fresh_db().await?;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let clock = Arc::new(FakeClock::new(CLOCK_START_MS));
    let fetcher = Arc::new(StaticFetcher::new());
    fetcher.serve(&source('1'), &endpoint_manifest(&["run.cancel"]));
    fetcher.serve(&plain_source(), &plain_manifest());
    let st = state(&pg, &clock, &fetcher, &[PUBLISHER]);
    let public = router(st.clone());
    let broker = broker::router(st.clone());
    install(&public, "mcp", &source('1'), &image('a')).await;
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
    let mcp = PluginStore::installation(&*pg, "acme", "mcp")
        .await
        .unwrap()
        .unwrap();
    Some(World {
        tdb,
        pg,
        clock,
        fetcher,
        st,
        public,
        broker,
        mcp,
    })
}

async fn send(app: &axum::Router, req: Request<Body>) -> Response {
    app.clone().oneshot(req).await.unwrap()
}

async fn read(resp: Response) -> (StatusCode, String) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// A public API call with no credential (authz is off).
async fn admin(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let (status, text) = read(send(app, req).await).await;
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, value)
}

/// Fetch `source` for review, then install it as `name` from `image`.
async fn install(app: &axum::Router, name: &str, source: &str, image: &str) {
    let (status, fetched) = admin(
        app,
        "POST",
        "/v1/orgs/acme/plugin-fetch",
        Some(json!({ "source": source })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    let (status, body) = admin(
        app,
        "POST",
        "/v1/orgs/acme/plugins",
        Some(json!({
            "name": name,
            "image": image,
            "source": source,
            "manifest_sha256": fetched["manifest_sha256"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Upgrade `mcp` to the manifest at commit `c`, on `image`.
async fn upgrade(w: &World, c: char, image: &str) {
    let (status, fetched) = admin(
        &w.public,
        "POST",
        "/v1/orgs/acme/plugins/mcp/upgrade-fetch",
        Some(json!({ "commit": c.to_string().repeat(40) })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    let (status, body) = admin(
        &w.public,
        "POST",
        "/v1/orgs/acme/plugins/mcp/upgrade",
        Some(json!({
            "image": image,
            "source": fetched["source"],
            "manifest_sha256": fetched["manifest_sha256"],
            "expected_current_sha256": fetched["current_manifest_sha256"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// An inline Run, stamped into `org`'s project `web`.
async fn seed_run(w: &World, org: &str) -> RunId {
    let (status, body) = admin(
        &w.public,
        "POST",
        "/v1/runs",
        Some(json!({
            "pipeline": {
                "ir_version": 1,
                "steps": [{ "id": "b", "image": "busybox", "command": ["true"] }]
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let run = RunId(body["id"].as_str().unwrap().to_string());
    Db::set_run_tenant(&*w.pg, &run, org, "web").await.unwrap();
    run
}

/// The URL of `installation`'s endpoint `path`, as an `oauth` token's
/// audience names it.
fn audience(installation: &PluginInstallation, path: &str) -> String {
    format!(
        "https://scarab.example/v1/orgs/{}/plugin-installations/{}/endpoints/{path}",
        installation.org, installation.id
    )
}

/// amy's token `id`: `oauth`, Member ceiling, in `scope`, at `audience`.
fn oauth_token(
    id: &str,
    scope: Scope,
    audience: &str,
    operations: Option<&[&str]>,
) -> PrincipalToken {
    PrincipalToken {
        id: id.into(),
        name: format!("claude.ai {id}"),
        owner_subject: "amy".into(),
        scope,
        role: Role::Member,
        expires_at: CLOCK_START_MS + HOUR_MS,
        created_by: "amy".into(),
        created_at: CLOCK_START_MS,
        last_used_at: None,
        use_count: 0,
        revoked_at: None,
        kind: PrincipalTokenKind::Oauth,
        operations: operations.map(|ops| ops.iter().map(|o| o.to_string()).collect()),
        audience: Some(audience.into()),
        oauth_client: Some("https://claude.ai/oauth/mcp-client-metadata".into()),
    }
}

async fn put_token(pg: &PostgresDb, token: &PrincipalToken) {
    let hash = token.stored_hash(&format!("plaintext-{}", token.id));
    ApiTokenStore::put(pg, token, &hash).await.unwrap();
}

/// Mint a handle through the server's own mint, for the endpoint
/// operations `names`.
async fn mint(
    w: &World,
    installation: &PluginInstallation,
    token: &PrincipalToken,
    names: &[&str],
) -> (String, DelegatedCallerHandle) {
    match mint_delegated_caller_handle(&w.st, installation, token, names).await {
        Ok(minted) => minted,
        Err(_) => panic!("the mint refused {names:?} for {}", token.id),
    }
}

/// Write a handle row directly, bypassing the mint's checks: what the
/// middleware must refuse on its own.
async fn put_handle(
    pg: &PostgresDb,
    id: &str,
    installation: &PluginInstallation,
    org: &str,
    token_id: &str,
) -> String {
    let value = format!("scarab_dch_{id}");
    let handle = DelegatedCallerHandle {
        id: id.into(),
        installation_id: installation.id.clone(),
        org: org.into(),
        token_id: token_id.into(),
        operations: vec!["run.cancel".into(), "run.read".into()],
        writes_left: 1,
        calls_left: 50,
        created_at: CLOCK_START_MS,
        heartbeat_at: CLOCK_START_MS,
        expires_at: CLOCK_START_MS + 10 * 60 * 1000,
    };
    DelegatedCallStore::put_delegated_caller_handle(
        pg,
        &handle,
        &delegated_caller_handle_hash(&value),
    )
    .await
    .unwrap();
    value
}

/// A PluginApi request from `installation`'s token with `handle`.
fn delegated(
    method: &str,
    uri: &str,
    installation: Option<&str>,
    handle: Option<&str>,
) -> Request<Body> {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(id) = installation {
        req = req.header("authorization", format!("Bearer tok-{id}"));
    }
    if let Some(handle) = handle {
        req = req.header(DELEGATED_CALLER_HANDLE_HEADER, handle);
    }
    req.body(Body::empty()).unwrap()
}

/// `mcp`'s call with `handle`, answered.
async fn call(w: &World, method: &str, uri: &str, handle: &str) -> (StatusCode, String) {
    let req = delegated(method, uri, Some(w.mcp.id.as_str()), Some(handle));
    read(send(&w.broker, req).await).await
}

/// The journal, oldest first: (operation, status).
async fn journal(pool: &PgPool) -> Vec<(String, Option<i32>)> {
    sqlx::query_as("SELECT operation, status FROM delegated_call_journal ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// A handle's (calls left, writes left).
async fn counters(pool: &PgPool, handle_id: &str) -> (i32, i32) {
    sqlx::query_as("SELECT calls_left, writes_left FROM delegated_caller_handles WHERE id = $1")
        .bind(handle_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// How many handles installation `id` has open.
async fn open_handles(pool: &PgPool, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM delegated_caller_handles WHERE installation_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The cancel requests `run`'s event log records.
async fn cancel_requests(
    pg: &PostgresDb,
    run: &RunId,
) -> Vec<(Option<String>, Option<scarab_engine::DelegationTrail>)> {
    Db::events(pg, run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::RunCancelRequested { by, via } => Some((by, via)),
            _ => None,
        })
        .collect()
}

/// A handle reads and writes as amy: her live role in her own org, the Run
/// of another org refused by the handler's own authorization; the cancel
/// event's trail names the token and the installation; each call is
/// journaled with the server's request id beside the plugin's (cut to 128).
/// The mint narrows `may` to the token's own operation list and binds the
/// handle's life to ten minutes.
#[tokio::test]
async fn a_delegated_call_acts_as_its_caller_and_is_journaled() {
    let Some(w) = world().await else {
        return;
    };
    let run = seed_run(&w, "acme").await;
    let elsewhere = seed_run(&w, "globex").await;
    let token = oauth_token(
        "t-amy",
        Scope::Org("acme".into()),
        &audience(&w.mcp, "mcp"),
        None,
    );
    put_token(&w.pg, &token).await;
    let (handle, minted) = mint(&w, &w.mcp, &token, &["mcp/read_run", "mcp/cancel_run"]).await;
    assert!(handle.starts_with("scarab_dch_"), "{handle}");
    assert_eq!(minted.operations, vec!["run.cancel", "run.read"]);
    assert_eq!((minted.calls_left, minted.writes_left), (50, 1));
    assert_eq!(minted.expires_at, CLOCK_START_MS + 10 * 60 * 1000);

    let mut req = delegated(
        "GET",
        &format!("/v1/runs/{}", run.0),
        Some(w.mcp.id.as_str()),
        Some(handle.as_str()),
    );
    req.headers_mut()
        .insert("x-request-id", "r".repeat(200).parse().unwrap());
    let (status, body) = read(send(&w.broker, req).await).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = call(&w, "GET", &format!("/v1/runs/{}", elsewhere.0), &handle).await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
        "another org's Run: {status} {body}"
    );

    let (status, body) = call(&w, "POST", &format!("/v1/runs/{}/cancel", run.0), &handle).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let cancels = cancel_requests(&w.pg, &run).await;
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    let (by, via) = &cancels[0];
    assert_eq!(by.as_deref(), Some("amy"));
    let via = via.as_ref().expect("a delegated write records its trail");
    assert_eq!(via.token_id, "t-amy");
    assert_eq!(via.token_kind, "oauth");
    assert_eq!(via.plugin_installation.as_deref(), Some(w.mcp.id.as_str()));
    assert_eq!(via.oauth_client, token.oauth_client);

    let rows = journal(&w.tdb.pool).await;
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0], ("run.read".to_string(), Some(200)));
    assert_eq!(rows[2], ("run.cancel".to_string(), Some(202)));
    let (ours, theirs): (String, Option<String>) = sqlx::query_as(
        "SELECT request_id, plugin_request_id FROM delegated_call_journal ORDER BY id LIMIT 1",
    )
    .fetch_one(&w.tdb.pool)
    .await
    .unwrap();
    assert_eq!(theirs, Some("r".repeat(128)));
    assert!(!ours.is_empty() && !ours.starts_with('r'), "{ours}");
    assert_eq!(counters(&w.tdb.pool, &minted.id).await, (47, 0));

    // A token whose own list lacks `run.cancel` gets a handle without it,
    // whatever the endpoint operation may use.
    let narrow = oauth_token(
        "t-amy-read",
        Scope::Org("acme".into()),
        &audience(&w.mcp, "mcp"),
        Some(&["run.read"][..]),
    );
    put_token(&w.pg, &narrow).await;
    let (handle, minted) = mint(&w, &w.mcp, &narrow, &["mcp/read_run", "mcp/cancel_run"]).await;
    assert_eq!(minted.operations, vec!["run.read"]);
    let other = seed_run(&w, "acme").await;
    let (status, body) = call(&w, "POST", &format!("/v1/runs/{}/cancel", other.0), &handle).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("operation_not_granted"), "{body}");
    assert!(cancel_requests(&w.pg, &other).await.is_empty());
    w.cleanup().await;
}

/// Every refusal before the handle and its token are proven is a 401 (403
/// for an installation that may not call PluginApi at all) and leaves no
/// journal row: no installation token, a token that maps to nothing, no
/// handle, a value without the prefix, an unknown handle, another
/// installation's handle, a handle row of another org, an issued token, an
/// `oauth` token for another installation's endpoint or with a query, one of
/// another org, a revoked one, a stale heartbeat. A gate approve is not
/// served at all.
#[tokio::test]
async fn plugin_api_refuses_an_unproven_installation_handle_or_token_before_any_handler() {
    let Some(w) = world().await else {
        return;
    };
    let run = seed_run(&w, "acme").await;
    let uri = format!("/v1/runs/{}", run.0);
    install(&w.public, "mcp2", &source('1'), &image('a')).await;
    install(&w.public, "plain", &plain_source(), &image('a')).await;
    let mcp2 = w.installation("mcp2").await;
    let plain = w.installation("plain").await;
    let acme = || Scope::Org("acme".into());

    let good = oauth_token("t-good", acme(), &audience(&w.mcp, "mcp"), None);
    let for_mcp2 = oauth_token("t-mcp2", acme(), &audience(&mcp2, "mcp"), None);
    let with_query = oauth_token(
        "t-query",
        acme(),
        &format!("{}?x=1", audience(&w.mcp, "mcp")),
        None,
    );
    let other_org = oauth_token(
        "t-globex",
        Scope::Org("globex".into()),
        &audience(&w.mcp, "mcp"),
        None,
    );
    let revoked = oauth_token("t-revoked", acme(), &audience(&w.mcp, "mcp"), None);
    let mut issued = oauth_token("t-issued", acme(), "", None);
    issued.kind = PrincipalTokenKind::Issued;
    issued.audience = None;
    issued.oauth_client = None;
    for token in [&good, &for_mcp2, &with_query, &other_org, &revoked, &issued] {
        put_token(&w.pg, token).await;
    }
    ApiTokenStore::revoke(&*w.pg, "t-revoked", CLOCK_START_MS)
        .await
        .unwrap();

    let good_handle = put_handle(&w.pg, "h-good", &w.mcp, "acme", "t-good").await;
    let mcp2_handle = put_handle(&w.pg, "h-mcp2", &mcp2, "acme", "t-mcp2").await;
    let rows: Vec<(&str, Request<Body>, StatusCode)> = vec![
        (
            "no installation token",
            delegated("GET", &uri, None, Some(good_handle.as_str())),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "an installation token that maps to nothing",
            delegated("GET", &uri, Some("0000000000"), Some(good_handle.as_str())),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "no handle",
            delegated("GET", &uri, Some(w.mcp.id.as_str()), None),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "a value without the prefix",
            delegated(
                "GET",
                &uri,
                Some(w.mcp.id.as_str()),
                Some(good_handle.trim_start_matches("scarab_dch_")),
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "an unknown handle",
            delegated(
                "GET",
                &uri,
                Some(w.mcp.id.as_str()),
                Some("scarab_dch_nope"),
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "another installation's handle",
            delegated(
                "GET",
                &uri,
                Some(w.mcp.id.as_str()),
                Some(mcp2_handle.as_str()),
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "an installation with no endpoint",
            delegated(
                "GET",
                &uri,
                Some(plain.id.as_str()),
                Some(good_handle.as_str()),
            ),
            StatusCode::FORBIDDEN,
        ),
        (
            "a gate approve, never served",
            delegated(
                "POST",
                &format!("/v1/runs/{}/gates/b/approve", run.0),
                Some(w.mcp.id.as_str()),
                Some(good_handle.as_str()),
            ),
            StatusCode::NOT_FOUND,
        ),
    ];
    for (why, req, expected) in rows {
        let (status, body) = read(send(&w.broker, req).await).await;
        assert_eq!(status, expected, "{why}: {body}");
    }

    // Handles whose row or token is wrong, each on mcp's own token.
    for (why, handle_id, org, token_id) in [
        ("a handle row of another org", "h-org", "globex", "t-good"),
        ("an issued token", "h-issued", "acme", "t-issued"),
        (
            "an oauth token for another installation's endpoint",
            "h-aud",
            "acme",
            "t-mcp2",
        ),
        ("an audience with a query", "h-query", "acme", "t-query"),
        ("a token of another org", "h-globex", "acme", "t-globex"),
        ("a revoked token", "h-revoked", "acme", "t-revoked"),
        ("an unknown token", "h-unknown", "acme", "t-nope"),
    ] {
        let handle = put_handle(&w.pg, handle_id, &w.mcp, org, token_id).await;
        let (status, body) = call(&w, "GET", &uri, &handle).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{why}: {body}");
    }

    // The good handle works, until its heartbeat is two minutes old.
    let (status, body) = call(&w, "GET", &uri, &good_handle).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    w.clock.advance(2 * 60 * 1000);
    let (status, body) = call(&w, "GET", &uri, &good_handle).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a stale heartbeat: {body}"
    );

    assert_eq!(
        journal(&w.tdb.pool).await,
        vec![("run.read".to_string(), Some(200))],
        "only the call that was made is journaled"
    );
    w.cleanup().await;
}

/// Once the handle and its token are proven, a refusal is journaled with its
/// status and the handler does not run: an operation the handle does not
/// hold (403), no write left (429), no call left (429), the installation's
/// broker permits all held by open responses (429). A call that cannot be
/// journaled is a 503, is not made, and spends nothing.
#[tokio::test]
async fn a_refusal_after_the_handle_is_proven_is_journaled_and_spends_nothing_it_should_not() {
    let Some(w) = world().await else {
        return;
    };
    let token = oauth_token(
        "t-amy",
        Scope::Org("acme".into()),
        &audience(&w.mcp, "mcp"),
        None,
    );
    put_token(&w.pg, &token).await;
    let (first, second, third) = (
        seed_run(&w, "acme").await,
        seed_run(&w, "acme").await,
        seed_run(&w, "acme").await,
    );

    // Not in the handle: a read-only handle cancels nothing.
    let (reader, _) = mint(&w, &w.mcp, &token, &["mcp/read_run"]).await;
    let (status, body) = call(&w, "POST", &format!("/v1/runs/{}/cancel", first.0), &reader).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // One write: the second is refused and its Run stays as it was.
    let (writer, minted) = mint(&w, &w.mcp, &token, &["mcp/read_run", "mcp/cancel_run"]).await;
    let (status, body) = call(&w, "POST", &format!("/v1/runs/{}/cancel", first.0), &writer).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, body) = call(
        &w,
        "POST",
        &format!("/v1/runs/{}/cancel", second.0),
        &writer,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(cancel_requests(&w.pg, &second).await.is_empty());

    // No call left.
    sqlx::query("UPDATE delegated_caller_handles SET calls_left = 0 WHERE id = $1")
        .bind(&minted.id)
        .execute(&w.tdb.pool)
        .await
        .unwrap();
    let (status, body) = call(&w, "GET", &format!("/v1/runs/{}", first.0), &writer).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    // Every broker permit of the installation held by a response whose body
    // is still open: the next call is refused, and goes through once they
    // close.
    let (reads, _) = mint(&w, &w.mcp, &token, &["mcp/read_run"]).await;
    let uri = format!("/v1/runs/{}", third.0);
    let mut open = Vec::new();
    for _ in 0..16 {
        let resp = send(
            &w.broker,
            delegated("GET", &uri, Some(w.mcp.id.as_str()), Some(reads.as_str())),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        open.push(resp);
    }
    let (status, body) = call(&w, "GET", &uri, &reads).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    drop(open);
    let (status, body) = call(&w, "GET", &uri, &reads).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The journal cannot be written: 503, the Run is not cancelled, and the
    // handle keeps its call and its write.
    let (late, late_handle) = mint(&w, &w.mcp, &token, &["mcp/cancel_run"]).await;
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_journal() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'journal unavailable'; END $$; \
         CREATE TRIGGER refuse_journal BEFORE INSERT ON delegated_call_journal \
         FOR EACH ROW EXECUTE FUNCTION refuse_journal();",
    )
    .execute(&w.tdb.pool)
    .await
    .unwrap();
    let (status, body) = call(&w, "POST", &format!("/v1/runs/{}/cancel", third.0), &late).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(cancel_requests(&w.pg, &third).await.is_empty());
    assert_eq!(counters(&w.tdb.pool, &late_handle.id).await, (50, 1));
    sqlx::raw_sql("DROP TRIGGER refuse_journal ON delegated_call_journal")
        .execute(&w.tdb.pool)
        .await
        .unwrap();

    let statuses: Vec<(String, Option<i32>)> = journal(&w.tdb.pool).await;
    let mut expected = vec![
        ("run.cancel".to_string(), Some(403)),
        ("run.cancel".to_string(), Some(202)),
        ("run.cancel".to_string(), Some(429)),
        ("run.read".to_string(), Some(429)),
    ];
    expected.extend(std::iter::repeat(("run.read".to_string(), Some(200))).take(16));
    expected.push(("run.read".to_string(), Some(429)));
    expected.push(("run.read".to_string(), Some(200)));
    assert_eq!(statuses, expected);
    w.cleanup().await;
}

/// ADR-0086 §8, §11: open handles end when the installation is killed (and
/// stay ended after it is enabled again), upgraded to a new digest, or
/// uninstalled; an upgrade that takes an operation out of `may` refuses it
/// on a handle already open; delisting the publisher refuses every call and
/// every mint at once. The mint also refuses a killed installation, an
/// endpoint operation the manifest does not declare, and another org's
/// token.
#[tokio::test]
async fn kill_upgrade_uninstall_and_delisting_end_open_handles() {
    let Some(w) = world().await else {
        return;
    };
    let run = seed_run(&w, "acme").await;
    let uri = format!("/v1/runs/{}", run.0);
    let token = oauth_token(
        "t-amy",
        Scope::Org("acme".into()),
        &audience(&w.mcp, "mcp"),
        None,
    );
    put_token(&w.pg, &token).await;

    let unknown = mint_delegated_caller_handle(&w.st, &w.mcp, &token, &["mcp/nope"]).await;
    assert!(matches!(unknown, Err(ApiError::BadRequest(_))));
    let globex = oauth_token(
        "t-globex",
        Scope::Org("globex".into()),
        &audience(&w.mcp, "mcp"),
        None,
    );
    put_token(&w.pg, &globex).await;
    let foreign = mint_delegated_caller_handle(&w.st, &w.mcp, &globex, &["mcp/read_run"]).await;
    assert!(matches!(foreign, Err(ApiError::Unauthorized)));

    // Delisted: the same database served by a server whose allowlist no
    // longer names the publisher.
    let delisted = state(&w.pg, &w.clock, &w.fetcher, &[]);
    let delisted_broker = broker::router(delisted.clone());
    let (handle, _) = mint(&w, &w.mcp, &token, &["mcp/read_run"]).await;
    let req = delegated("GET", &uri, Some(w.mcp.id.as_str()), Some(handle.as_str()));
    let (status, body) = read(send(&delisted_broker, req).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "delisted: {body}");
    let refused = mint_delegated_caller_handle(&delisted, &w.mcp, &token, &["mcp/read_run"]).await;
    assert!(matches!(refused, Err(ApiError::Denied(_))));
    let (status, body) = call(&w, "GET", &uri, &handle).await;
    assert_eq!(status, StatusCode::OK, "still listed here: {body}");

    // Kill, then enable: the handle open before the kill stays refused.
    let (status, body) = admin(&w.public, "POST", "/v1/orgs/acme/plugins/mcp/kill", None).await;
    assert!(status.is_success(), "{body}");
    assert_eq!(open_handles(&w.tdb.pool, &w.mcp.id).await, 0);
    let killed = w.installation("mcp").await;
    let refused = mint_delegated_caller_handle(&w.st, &killed, &token, &["mcp/read_run"]).await;
    assert!(matches!(refused, Err(ApiError::Denied(_))));
    let (status, body) = admin(&w.public, "POST", "/v1/orgs/acme/plugins/mcp/enable", None).await;
    assert!(status.is_success(), "{body}");
    let (status, body) = call(&w, "GET", &uri, &handle).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "after kill and enable: {body}"
    );

    // An upgrade on the same digest that takes `run.cancel` out of `may`:
    // a handle that holds it may no longer use it; reads go on.
    let mcp = w.installation("mcp").await;
    let (handle, _) = mint(&w, &mcp, &token, &["mcp/read_run", "mcp/cancel_run"]).await;
    w.fetcher.serve(&source('2'), &endpoint_manifest(&[]));
    upgrade(&w, '2', &image('a')).await;
    let (status, body) = call(&w, "POST", &format!("/v1/runs/{}/cancel", run.0), &handle).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "narrowed: {body}");
    assert!(cancel_requests(&w.pg, &run).await.is_empty());
    let (status, body) = call(&w, "GET", &uri, &handle).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A new digest is new code: the handles go.
    w.fetcher.serve(&source('3'), &endpoint_manifest(&[]));
    upgrade(&w, '3', &image('b')).await;
    assert_eq!(open_handles(&w.tdb.pool, &mcp.id).await, 0);
    let (status, body) = call(&w, "GET", &uri, &handle).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "after a new digest: {body}"
    );

    // Uninstall.
    let mcp = w.installation("mcp").await;
    mint(&w, &mcp, &token, &["mcp/read_run"]).await;
    assert_eq!(open_handles(&w.tdb.pool, &mcp.id).await, 1);
    let (status, body) = admin(&w.public, "DELETE", "/v1/orgs/acme/plugins/mcp", None).await;
    assert!(status.is_success(), "{body}");
    assert_eq!(open_handles(&w.tdb.pool, &mcp.id).await, 0);
    w.cleanup().await;
}
