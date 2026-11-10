//! Plugins end to end over HTTP (ADR-0072, MMP slice d1): an org admin
//! fetches a plugin's manifest from its git source, installs it by image
//! digest with the manifest hash they reviewed, and the plugin proves
//! who it is to the CredentialBroker's own router (`whoami`) — refused when
//! its token maps to nothing, and from the very next request once killed.
//! Report ingest went with the Deployment record (ADR-0074).
//!
//! Every plugin installs the same way, the Argo CD plugin included.
//!
//! Real Postgres for every store; the fakes are the token verifier (it
//! stands in for TokenReview, a true external — slice d2) and the forge a
//! manifest is read from (`StaticFetcher`, so no test makes a network call);
//! the clock is a `FakeClock`. Each test gets its own database (`fresh_db`) and skips
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{Binding, BindingOrigin, Principal, RbacStore, Role, Scope};
use scarab_plugin::{
    InstallationState, PluginStore, PluginTokenVerifier, RuntimeStatus, TokenRejected,
    VerifiedServiceAccount,
};
use scarab_server::plugins::PluginPolicy;
use scarab_server::test_support::{
    argocd_fetcher, argocd_install_body, manifest_sha256, StaticFetcher, ARGOCD_MANIFEST,
    ARGOCD_MANIFEST_SOURCE,
};
use scarab_server::{broker, router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, InMemoryDb, InMemoryObjectStore, InMemoryRbac, InMemorySessions,
};

const CLOCK_START_MS: i64 = 1_000_000;
const REPO: &str = "ghcr.io/thulasi-ram/scarab-plugin-argocd";
const NAMESPACE: &str = "scarab-plugins";

fn image() -> String {
    format!("{REPO}@sha256:{}", "a".repeat(64))
}

/// Stands in for TokenReview: `tok-<id>` is the projected token of
/// installation `<id>`'s ServiceAccount in the plugin namespace;
/// `foreign-<id>` is the same ServiceAccount name in another namespace;
/// anything else is rejected.
struct FakeTokenReview;

#[async_trait::async_trait]
impl PluginTokenVerifier for FakeTokenReview {
    async fn verify(&self, token: &str) -> Result<VerifiedServiceAccount, TokenRejected> {
        let (namespace, id) = if let Some(id) = token.strip_prefix("tok-") {
            (NAMESPACE, id)
        } else if let Some(id) = token.strip_prefix("foreign-") {
            ("default", id)
        } else {
            return Err(TokenRejected("unknown token".into()));
        };
        Ok(VerifiedServiceAccount {
            namespace: namespace.into(),
            service_account: format!("plugin-{id}"),
        })
    }
}

fn policy(enabled: bool) -> PluginPolicy {
    PluginPolicy {
        enabled,
        namespace: NAMESPACE.into(),
        dev_allow_tag: false,
    }
}

fn state(pg: Arc<PostgresDb>, enabled: bool) -> AppState {
    state_with(pg, enabled, Arc::new(argocd_fetcher()))
}

/// [`state`], reading manifests from `fetcher`.
fn state_with(pg: Arc<PostgresDb>, enabled: bool, fetcher: Arc<StaticFetcher>) -> AppState {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    AppState::new(db, Arc::new(FakeClock::new(CLOCK_START_MS)), logs)
        .with_org_environments(pg.clone())
        .with_plugins(pg, policy(enabled))
        .with_plugin_tokens(Arc::new(FakeTokenReview))
        .with_library_fetcher(fetcher)
}

/// The public API and the broker over one state, authz off (every caller an
/// owner).
struct Apps {
    public: axum::Router,
    broker: axum::Router,
}

fn apps(pg: Arc<PostgresDb>) -> Apps {
    apps_with(pg, Arc::new(argocd_fetcher()))
}

/// [`apps`], reading manifests from `fetcher`.
fn apps_with(pg: Arc<PostgresDb>, fetcher: Arc<StaticFetcher>) -> Apps {
    let st = state_with(pg, true, fetcher);
    Apps {
        public: router(st.clone()),
        broker: broker::router(st),
    }
}

async fn migrated() -> Option<(common::TestDb, Arc<PostgresDb>)> {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return None;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    Some((tdb, pg))
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(bearer) = bearer {
        req = req.header("authorization", format!("Bearer {bearer}"));
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

/// The install body for the Argo CD plugin as `name`.
fn argocd(name: &str) -> Value {
    argocd_install_body(name, &image())
}

/// `body` with `key` set to `value`.
fn with(mut body: Value, key: &str, value: Value) -> Value {
    body[key] = value;
    body
}

/// `POST …/plugin-fetch` of `source`, anonymously.
async fn fetch(app: &axum::Router, org: &str, source: &str) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/v1/orgs/{org}/plugin-fetch"),
        None,
        Some(json!({ "source": source })),
    )
    .await
}

/// The names of `org`'s installations.
async fn installed(app: &axum::Router, org: &str) -> Vec<String> {
    let (status, list) = call(app, "GET", &format!("/v1/orgs/{org}/plugins"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    list.as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect()
}

/// Install the Argo CD plugin as `name` and return its token.
async fn install(app: &axum::Router, org: &str, name: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/v1/orgs/{org}/plugins"),
        None,
        Some(argocd(name)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    format!("tok-{}", body["id"].as_str().unwrap())
}

async fn whoami(apps: &Apps, token: Option<&str>) -> (StatusCode, Value) {
    call(
        &apps.broker,
        "GET",
        "/_scarab/plugin/v1/whoami",
        token,
        None,
    )
    .await
}

/// Like [`call`], with the response headers.
async fn call_with_headers(
    app: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(bearer) = bearer {
        req = req.header("authorization", format!("Bearer {bearer}"));
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
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

#[tokio::test]
async fn a_bad_unmapped_or_foreign_token_is_401_and_healthz_needs_none() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg);
    let token = install(&apps.public, "acme", "argocd").await;
    let id = token.strip_prefix("tok-").unwrap();
    let (status, _) = call(
        &apps.broker,
        "GET",
        "/_scarab/plugin/v1/healthz",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for bearer in [
        None,
        Some("garbage".to_string()),
        // A ServiceAccount that maps to no installation (a reinstall's old
        // id, say).
        Some("tok-ffffffffff".to_string()),
        // The right ServiceAccount name, outside the plugin namespace.
        Some(format!("foreign-{id}")),
    ] {
        let (status, _) = whoami(&apps, bearer.as_deref()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer:?}");
    }
    let (status, me) = whoami(&apps, Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "the real token gets in");
    assert_eq!(me["name"], json!("argocd"));
    tdb.cleanup().await;
}

#[tokio::test]
async fn a_kill_refuses_the_very_next_request_and_enable_restores_it() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg);
    let token = install(&apps.public, "acme", "argocd").await;
    let (status, _) = whoami(&apps, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, killed) = call(
        &apps.public,
        "POST",
        "/v1/orgs/acme/plugins/argocd/kill",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{killed}");
    assert_eq!(killed["state"], json!("killed"));
    // The token itself is still valid: the refusal is the kill switch's.
    let (status, body) = whoami(&apps, Some(&token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, _) = call(
        &apps.public,
        "POST",
        "/v1/orgs/acme/plugins/argocd/enable",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = whoami(&apps, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    tdb.cleanup().await;
}

/// git-bug da0af59 (8): an installation the plugin runtime has held (its
/// isolation probe failed, or the runtime refused it) is refused by the
/// broker, though its state is still Enabled; a later Running restores it.
#[tokio::test]
async fn the_broker_refuses_an_installation_the_runtime_held() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg.clone());
    let token = install(&apps.public, "acme", "argocd").await;
    let id = token.strip_prefix("tok-").unwrap().to_string();
    assert_eq!(whoami(&apps, Some(&token)).await.0, StatusCode::OK);

    for held in [RuntimeStatus::ProbeFailed, RuntimeStatus::Error] {
        assert!(pg
            .set_runtime_status(&id, InstallationState::Enabled, 0, held, "held", None)
            .await
            .unwrap());
        let (status, body) = whoami(&apps, Some(&token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{held:?}: {body}");
        assert!(
            body.to_string().contains("held by the plugin runtime"),
            "{body}"
        );
    }
    assert!(pg
        .set_runtime_status(
            &id,
            InstallationState::Enabled,
            0,
            RuntimeStatus::Running,
            "",
            Some(1)
        )
        .await
        .unwrap());
    assert_eq!(whoami(&apps, Some(&token)).await.0, StatusCode::OK);
    tdb.cleanup().await;
}

/// git-bug da0af59 (8, red team): a workload that is merely failing (a
/// crash loop, an image pull, a quota refusal) records `error` with the
/// probe that let it run. It was never held, and the broker serves it.
#[tokio::test]
async fn a_failing_workload_is_not_a_held_installation() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg.clone());
    let token = install(&apps.public, "acme", "argocd").await;
    let id = token.strip_prefix("tok-").unwrap().to_string();
    assert!(pg
        .set_runtime_status(
            &id,
            InstallationState::Enabled,
            0,
            RuntimeStatus::Error,
            "CrashLoopBackOff: back-off 40s restarting failed container",
            Some(1),
        )
        .await
        .unwrap());
    let (status, body) = whoami(&apps, Some(&token)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a crash-looping Pod was never held: {body}"
    );
    tdb.cleanup().await;
}

/// A plugin is installed by image digest, whoever published it: a tag, a
/// digest of the wrong shape or a bare repository is a 400 and stores
/// nothing, and any repository is accepted by digest. With
/// SCARAB_PLUGINS=off every install is refused.
///
/// This test once asserted that every plugin but Argo CD from one repository
/// was refused. That allowlist is gone: what an admin approves, for every
/// plugin alike, is the image digest and the manifest they reviewed (see
/// the tests below for the manifest half).
#[tokio::test]
async fn a_plugin_installs_only_by_digest_and_only_while_plugins_are_on() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg.clone());
    for image in [
        format!("{REPO}:latest"),
        REPO.to_string(),
        format!("{REPO}@sha256:{}", "A".repeat(64)),
        format!("{REPO}@sha256:{}", "a".repeat(63)),
        format!("{REPO}@sha512:{}", "a".repeat(64)),
        format!("{REPO}:v1@sha256:{}", "a".repeat(64)),
    ] {
        let (status, body) = call(
            &apps.public,
            "POST",
            "/v1/orgs/acme/plugins",
            None,
            Some(argocd_install_body("p", &image)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{image}: {body}");
        assert!(body.to_string().contains("sha256"), "{body}");
    }
    assert_eq!(installed(&apps.public, "acme").await, Vec::<String>::new());

    // By digest, it installs from the repository its manifest names.
    let (status, body) = call(
        &apps.public,
        "POST",
        "/v1/orgs/acme/plugins",
        None,
        Some(argocd("argocd-a")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["image"], json!(image()));

    // With SCARAB_PLUGINS=off every install is refused.
    let off = router(state(pg, false));
    let (status, _) = call(
        &off,
        "POST",
        "/v1/orgs/acme/plugins",
        None,
        Some(argocd("argocd")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(installed(&off, "acme").await, vec!["argocd-a".to_string()]);
    tdb.cleanup().await;
}

/// The manifest names the plugin, so an install cannot: a body that still
/// carries `plugin`, or that lacks the source or the reviewed hash, is
/// refused before any source is read or anything stored.
#[tokio::test]
async fn an_install_body_that_names_the_plugin_or_skips_the_review_is_refused() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let fetcher = Arc::new(argocd_fetcher());
    let apps = apps_with(pg, fetcher.clone());
    for body in [
        with(argocd("argocd"), "plugin", json!("argocd")),
        with(argocd("argocd"), "plugin", json!("flux")),
        json!({ "name": "argocd", "image": image() }),
        json!({ "name": "argocd", "image": image(), "source": ARGOCD_MANIFEST_SOURCE }),
        json!({
            "name": "argocd",
            "image": image(),
            "manifest_sha256": manifest_sha256(ARGOCD_MANIFEST),
        }),
    ] {
        let (status, msg) = call(
            &apps.public,
            "POST",
            "/v1/orgs/acme/plugins",
            None,
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {msg}");
    }
    assert_eq!(fetcher.asked(), Vec::<String>::new(), "no source was read");
    assert_eq!(installed(&apps.public, "acme").await, Vec::<String>::new());
    tdb.cleanup().await;
}

/// The admin approves the manifest they reviewed, by its hash. An install
/// is refused with 409 when the source now reads as another manifest (one
/// read rule more), when the hash is one no fetch returned, or when the
/// reviewed hash is sent with another source. Nothing is stored; the
/// reviewed pair still installs.
#[tokio::test]
async fn an_install_whose_source_no_longer_reads_as_the_reviewed_manifest_is_409() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let fetcher = Arc::new(argocd_fetcher());
    let apps = apps_with(pg, fetcher.clone());
    let (status, fetched) = fetch(&apps.public, "acme", ARGOCD_MANIFEST_SOURCE).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    let reviewed = fetched["manifest_sha256"].as_str().unwrap().to_string();
    assert_eq!(reviewed, manifest_sha256(ARGOCD_MANIFEST));

    let mut wider: Value = serde_json::from_str(ARGOCD_MANIFEST).unwrap();
    wider["connections"][0]["read_rules"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "method": "GET", "path": "/api/v1/account" }));
    let wider = wider.to_string();
    let other_source = format!(
        "github.com/someone-else/plugins//argocd.json@{}",
        "d".repeat(40)
    );
    fetcher.serve(&other_source, &wider);

    // The same source now serves a manifest that asks for more.
    fetcher.serve(ARGOCD_MANIFEST_SOURCE, &wider);
    let (status, body) = call(
        &apps.public,
        "POST",
        "/v1/orgs/acme/plugins",
        None,
        Some(argocd("argocd")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("you reviewed"), "{body}");
    fetcher.serve(ARGOCD_MANIFEST_SOURCE, ARGOCD_MANIFEST);

    for body in [
        // A hash no fetch returned.
        with(argocd("argocd"), "manifest_sha256", json!("0".repeat(64))),
        // The reviewed hash, with a source that reads as another manifest.
        with(argocd("argocd"), "source", json!(other_source)),
    ] {
        let (status, msg) = call(
            &apps.public,
            "POST",
            "/v1/orgs/acme/plugins",
            None,
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}: {msg}");
    }
    assert_eq!(installed(&apps.public, "acme").await, Vec::<String>::new());

    let (status, body) = call(
        &apps.public,
        "POST",
        "/v1/orgs/acme/plugins",
        None,
        Some(argocd("argocd")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    tdb.cleanup().await;
}

/// A manifest that asks for a capability this Scarab does not admit is a
/// 400 at fetch and at install, whoever published it, and names the
/// capability. So is a file that is not a manifest (a write rule).
#[tokio::test]
async fn a_manifest_asking_for_a_capability_not_admitted_is_refused_at_fetch_and_install() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let fetcher = Arc::new(StaticFetcher::new());
    let apps = apps_with(pg, fetcher.clone());
    for capability in [
        "approve_gates",
        "report_deployments",
        "subscribe_events",
        "offer_actions",
        "contribute_ui",
    ] {
        let mut m: Value = serde_json::from_str(ARGOCD_MANIFEST).unwrap();
        m["capabilities"]
            .as_array_mut()
            .unwrap()
            .push(json!(capability));
        let manifest = m.to_string();
        let source = format!(
            "github.com/someone-else/plugins//{capability}.json@{}",
            "c".repeat(40)
        );
        fetcher.serve(&source, &manifest);

        let (status, body) = fetch(&apps.public, "acme", &source).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{capability}: {body}");
        assert!(body.to_string().contains("cannot be installed"), "{body}");
        assert!(body.to_string().contains(capability), "{body}");

        let (status, body) = call(
            &apps.public,
            "POST",
            "/v1/orgs/acme/plugins",
            None,
            Some(json!({
                "name": "p",
                "image": image(),
                "source": source,
                "manifest_sha256": manifest_sha256(&manifest),
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{capability}: {body}");
    }

    let mut m: Value = serde_json::from_str(ARGOCD_MANIFEST).unwrap();
    m["connections"][0]["write_rules"] = json!([{ "method": "POST", "path": "/x" }]);
    let source = format!(
        "github.com/someone-else/plugins//writes.json@{}",
        "c".repeat(40)
    );
    fetcher.serve(&source, &m.to_string());
    let (status, body) = fetch(&apps.public, "acme", &source).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("not a plugin manifest"), "{body}");
    let (status, body) = call(
        &apps.public,
        "POST",
        "/v1/orgs/acme/plugins",
        None,
        Some(json!({
            "name": "p",
            "image": image(),
            "source": source,
            "manifest_sha256": "0".repeat(64),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    assert_eq!(installed(&apps.public, "acme").await, Vec::<String>::new());
    tdb.cleanup().await;
}

/// git-bug 97223c7: no plugin is installed into a `_`-prefixed org —
/// those are Scarab's own credential scopes, not Orgs.
#[tokio::test]
async fn a_plugin_is_not_installed_into_a_reserved_org() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg);
    for org in ["_plugin_connections", "_forge"] {
        let (status, body) = call(
            &apps.public,
            "POST",
            &format!("/v1/orgs/{org}/plugins"),
            None,
            Some(argocd("argocd")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{org}: {body}");
        assert!(body.to_string().contains("reserved"), "{body}");
        let (_, list) = call(
            &apps.public,
            "GET",
            &format!("/v1/orgs/{org}/plugins"),
            None,
            None,
        )
        .await;
        assert_eq!(list, json!([]), "{org}");
    }
    tdb.cleanup().await;
}

/// An uninstall retires the name: it can never be installed again in the org.
#[tokio::test]
async fn an_uninstall_retires_the_name() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg);
    let token = install(&apps.public, "acme", "argocd").await;
    let (status, uninstalled) = call(
        &apps.public,
        "DELETE",
        "/v1/orgs/acme/plugins/argocd",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{uninstalled}");
    assert_eq!(uninstalled, json!({ "name": "argocd" }));
    let (status, _) = whoami(&apps, Some(&token)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "its token maps to nothing"
    );
    let (status, _) = call(
        &apps.public,
        "POST",
        "/v1/orgs/acme/plugins",
        None,
        Some(argocd("argocd")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "the name is retired");
    tdb.cleanup().await;
}

/// The settings page's URL carries an installation's id: the id reads the
/// installation back in its own org, an id of another org's installation is
/// a 404 there, and so is the id of an uninstalled one.
#[tokio::test]
async fn an_installation_is_read_by_its_id_in_its_own_org_only() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg);
    let acme = install(&apps.public, "acme", "argocd").await;
    let globex = install(&apps.public, "globex", "argocd").await;
    let (acme, globex) = (
        acme.trim_start_matches("tok-").to_string(),
        globex.trim_start_matches("tok-").to_string(),
    );
    let get = |org: &'static str, id: String| {
        let app = apps.public.clone();
        async move {
            call(
                &app,
                "GET",
                &format!("/v1/orgs/{org}/plugin-installations/{id}"),
                None,
                None,
            )
            .await
        }
    };

    let (status, one) = get("acme", acme.clone()).await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["id"], json!(acme));
    assert_eq!(one["name"], json!("argocd"));
    let (status, _) = get("acme", globex.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "another org's id");
    let (status, _) = get("globex", acme.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "another org's id");
    let (status, _) = get("acme", "0000000000".into()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an id that names nothing");

    let (status, _) = call(
        &apps.public,
        "DELETE",
        "/v1/orgs/acme/plugins/argocd",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get("acme", acme).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an uninstalled one");
    tdb.cleanup().await;
}

/// ADR-0074 §7: an installation's storage claim is set at install, changed,
/// cleared and read back over the API, on real Postgres. A shape the runtime
/// could not mount safely is a 400 and stores nothing.
#[tokio::test]
async fn storage_is_set_at_install_changed_and_cleared() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let app = apps(pg).public;
    let plugin = "/v1/orgs/acme/plugins/argocd";
    let storage_uri = format!("{plugin}/storage");

    // A bad claim or mount path refuses the whole install.
    for storage in [
        json!({ "claim": "Not_A_Claim" }),
        json!({ "claim": "argocd-state", "mount_path": "/tmp/state" }),
        json!({ "claim": "argocd-state", "mount_path": "/var/run/secrets" }),
        json!({ "claim": "argocd-state", "mount_path": "/" }),
        json!({ "claim": "argocd-state", "mount_path": "data" }),
    ] {
        let (status, body) = call(
            &app,
            "POST",
            "/v1/orgs/acme/plugins",
            None,
            Some(with(argocd("argocd"), "storage", storage.clone())),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{storage}: {body}");
    }
    let (status, _) = call(&app, "GET", plugin, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing was installed");

    // Installed with a claim and the default mount path.
    let (status, body) = call(
        &app,
        "POST",
        "/v1/orgs/acme/plugins",
        None,
        Some(with(
            argocd("argocd"),
            "storage",
            json!({ "claim": "argocd-state" }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let want = json!({
        "claim": "argocd-state",
        "mount_path": "/data",
        "owner_annotation": "scarab.io/plugin-installation",
        "owner_annotation_value": "acme/argocd",
    });
    assert_eq!(body["storage"], want);
    let (_, body) = call(&app, "GET", plugin, None, None).await;
    assert_eq!(body["storage"], want, "read back from Postgres");

    // Changed: another claim and path.
    let (status, body) = call(
        &app,
        "PUT",
        &storage_uri,
        None,
        Some(json!({ "claim": "argocd-state-v2", "mount_path": "/var/lib/argocd/" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["storage"]["claim"], "argocd-state-v2");
    assert_eq!(body["storage"]["mount_path"], "/var/lib/argocd");

    // A bad change is refused and changes nothing.
    let (status, _) = call(
        &app,
        "PUT",
        &storage_uri,
        None,
        Some(json!({ "claim": "argocd-state", "mount_path": "/tmp" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, body) = call(&app, "GET", plugin, None, None).await;
    assert_eq!(body["storage"]["claim"], "argocd-state-v2");

    // Cleared, and clearing again is a no-op.
    for _ in 0..2 {
        let (status, body) = call(&app, "DELETE", &storage_uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["storage"], Value::Null);
    }
    let (_, body) = call(&app, "GET", plugin, None, None).await;
    assert_eq!(body["storage"], Value::Null);

    // No such installation.
    let (status, _) = call(
        &app,
        "PUT",
        "/v1/orgs/acme/plugins/nope/storage",
        None,
        Some(json!({ "claim": "argocd-state" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        "DELETE",
        "/v1/orgs/acme/plugins/nope/storage",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    tdb.cleanup().await;
}

/// Fetching, installing, listing and killing need Administer on the Org.
#[tokio::test]
async fn a_member_cannot_fetch_install_list_or_kill() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let scoped = |subject: &str| Principal {
        subject: subject.into(),
        handle: String::new(),
        display_name: None,
        roles: vec![],
    };
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential("ada-code", scoped("ada"))
            .with_credential("mel-code", scoped("mel")),
    );
    let rbac = Arc::new(InMemoryRbac::new());
    for (subject, role) in [("ada", Role::Admin), ("mel", Role::Member)] {
        rbac.grant(
            &Binding {
                subject: subject.into(),
                scope: Scope::Org("acme".into()),
                role,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    }
    let app = router(
        state(pg, true)
            .with_auth(auth, Arc::new(InMemorySessions::new()))
            .with_rbac(rbac),
    );
    let session = |credential: &'static str| {
        let app = app.clone();
        async move {
            let (_, body) = call(
                &app,
                "POST",
                "/v1/auth/login",
                None,
                Some(json!({ "credential": credential })),
            )
            .await;
            body["session"].as_str().unwrap().to_string()
        }
    };
    let (ada, mel) = (session("ada-code").await, session("mel-code").await);
    let body = || Some(argocd("argocd"));

    let (status, _) = call(&app, "POST", "/v1/orgs/acme/plugins", Some(&mel), body()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a member installs");
    let (status, _) = call(&app, "GET", "/v1/orgs/acme/plugins", Some(&mel), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a member lists");
    let source = Some(json!({ "source": ARGOCD_MANIFEST_SOURCE }));
    let (status, _) = call(
        &app,
        "POST",
        "/v1/orgs/acme/plugin-fetch",
        Some(&mel),
        source.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a member fetches");
    let (status, fetched) = call(
        &app,
        "POST",
        "/v1/orgs/acme/plugin-fetch",
        Some(&ada),
        source,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    let (status, installed) = call(&app, "POST", "/v1/orgs/acme/plugins", Some(&ada), body()).await;
    assert_eq!(status, StatusCode::CREATED, "{installed}");
    assert_eq!(installed["approved_by"], json!("ada"));
    let (status, _) = call(
        &app,
        "POST",
        "/v1/orgs/acme/plugins/argocd/kill",
        Some(&mel),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a member kills");
    let (status, _) = call(
        &app,
        "GET",
        &format!(
            "/v1/orgs/acme/plugin-installations/{}",
            installed["id"].as_str().unwrap()
        ),
        Some(&mel),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a member reads one by id");
    tdb.cleanup().await;
}

/// Red-team F-5: a broker 500 tells the plugin only its request id; the
/// store's own words go to the server log.
#[tokio::test]
async fn a_broker_500_carries_no_internal_detail() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg);
    let token = install(&apps.public, "acme", "argocd").await;
    // Break the store for real: the installation read now fails.
    sqlx::query("ALTER TABLE plugin_installations RENAME COLUMN manifest TO manifest_gone")
        .execute(&tdb.pool)
        .await
        .unwrap();
    let (status, headers, body) = call_with_headers(
        &apps.broker,
        "GET",
        "/_scarab/plugin/v1/whoami",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    let id = headers["x-request-id"].to_str().unwrap();
    assert_eq!(body, format!("internal error; request id {id}"));
    tdb.cleanup().await;
}

/// `state` with step-up ENFORCED (ADR-0073), over in-memory step-up stores.
fn enforced(st: AppState) -> AppState {
    st.with_step_up(scarab_server::step_up::StepUpService::new(
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
        scarab_webauthn::RelyingParty::from_public_url("https://ci.example.com", "Scarab").unwrap(),
    ))
}

/// ADR-0073 C5: with step-up ENFORCED the plugin's broker calls are exactly
/// as before — the CredentialBroker is its own listener, a PluginPrincipal
/// never meets the step-up guard — while the very same state's public API
/// does enforce it.
#[tokio::test]
async fn under_enforced_step_up_the_broker_is_unchanged() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let off = state(pg, true);
    let on = enforced(off.clone());
    assert!(on.step_up.is_enforced());
    // The public API of the enforced state refuses the crown jewel …
    let (status, body) = call(
        &router(on.clone()),
        "POST",
        "/v1/orgs/acme/plugins",
        None,
        Some(argocd("argocd-prod")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], json!("step_up_required"));
    assert_eq!(
        installed(&router(off.clone()), "acme").await,
        Vec::<String>::new(),
        "nothing is installed without the step-up"
    );
    // Reading a manifest to review installs nothing, so it needs no step-up.
    let (status, fetched) = fetch(&router(on.clone()), "acme", ARGOCD_MANIFEST_SOURCE).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    // … so the admin side is set up with it off (it is not under test).
    let apps = Apps {
        public: router(off),
        broker: broker::router(on),
    };
    let token = install(&apps.public, "acme", "argocd-prod").await;

    let (status, whoami) = call(
        &apps.broker,
        "GET",
        "/_scarab/plugin/v1/whoami",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{whoami}");
    assert_eq!(whoami["principal"], json!("plugin:argocd-prod"));
    let (status, healthz) = call(
        &apps.broker,
        "GET",
        "/_scarab/plugin/v1/healthz",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{healthz}");
    tdb.cleanup().await;
}

/// One journal row of installation `id`, as the broker would write it.
fn journal_row(
    id: &str,
    phase: scarab_plugin::BrokerJournalPhase,
    started_at: i64,
    query: Option<&str>,
    denied_reason: Option<&str>,
) -> scarab_plugin::BrokerJournalEntry {
    scarab_plugin::BrokerJournalEntry {
        request_id: format!("req-{started_at}"),
        phase,
        installation_id: id.into(),
        slot: "argocd".into(),
        method: "GET".into(),
        path: format!("/api/v1/applications/app-{started_at}"),
        upstream_path: Some(format!("/api/v1/applications/app-{started_at}")),
        query: query.map(str::to_string),
        rule: Some("GET /api/v1/applications/*".into()),
        status: Some(200),
        bytes: None,
        started_at,
        duration_ms: Some(3),
        denied_reason: denied_reason.map(str::to_string),
    }
}

/// An org admin reads an installation's broker journal newest first, a page
/// at a time, narrowed by phase or to denied rows, with query keys but never
/// query values. A member is refused; another org's installation is not
/// found.
#[tokio::test]
async fn an_org_admin_reads_the_broker_journal_newest_first_by_page() {
    use scarab_plugin::BrokerJournalPhase::{End, Head, Refused};
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let scoped = |subject: &str| Principal {
        subject: subject.into(),
        handle: String::new(),
        display_name: None,
        roles: vec![],
    };
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential("ada-code", scoped("ada"))
            .with_credential("mel-code", scoped("mel")),
    );
    let rbac = Arc::new(InMemoryRbac::new());
    for (subject, role) in [("ada", Role::Admin), ("mel", Role::Member)] {
        rbac.grant(
            &Binding {
                subject: subject.into(),
                scope: Scope::Org("acme".into()),
                role,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    }
    let app = router(
        state(pg.clone(), true)
            .with_auth(auth, Arc::new(InMemorySessions::new()))
            .with_rbac(rbac),
    );
    let session = |credential: &'static str| {
        let app = app.clone();
        async move {
            let (_, body) = call(
                &app,
                "POST",
                "/v1/auth/login",
                None,
                Some(json!({ "credential": credential })),
            )
            .await;
            body["session"].as_str().unwrap().to_string()
        }
    };
    let (ada, mel) = (session("ada-code").await, session("mel-code").await);
    let (status, installed) = call(
        &app,
        "POST",
        "/v1/orgs/acme/plugins",
        Some(&ada),
        Some(argocd("argocd")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{installed}");
    let acme = installed["id"].as_str().unwrap().to_string();
    let open = apps(pg.clone());
    let globex = install(&open.public, "globex", "globex-argo").await;
    let globex = globex.strip_prefix("tok-").unwrap().to_string();

    // Oldest first.
    pg.append_broker_journal(&[
        journal_row(&acme, End, 1, Some("projects=prod"), None),
        journal_row(
            &acme,
            Refused,
            2,
            Some("token=s3cret&projects=prod"),
            Some("query"),
        ),
        journal_row(&globex, End, 2, None, None),
        journal_row(&acme, Head, 3, None, None),
        journal_row(&acme, End, 4, None, Some("cut: killed")),
        journal_row(&acme, Refused, 5, None, Some("no read rule")),
    ])
    .await
    .unwrap();

    let read = |query: &str| {
        let (app, ada) = (app.clone(), ada.clone());
        let uri = format!("/v1/orgs/acme/plugins/argocd/broker-journal{query}");
        async move {
            let (status, body) = call(&app, "GET", &uri, Some(&ada), None).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            body
        }
    };
    let started = |page: &Value| -> Vec<i64> {
        page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["started_at"].as_i64().unwrap())
            .collect()
    };

    let all = read("").await;
    assert_eq!(started(&all), vec![5, 4, 3, 2, 1]);
    assert_eq!(all["next_before"], Value::Null);
    let refused = &all["entries"][3];
    assert_eq!(refused["phase"], json!("refused"));
    assert_eq!(refused["denied_reason"], json!("query"));
    assert_eq!(refused["query_keys"], json!(["token", "projects"]));
    assert!(
        !all.to_string().contains("s3cret"),
        "a query value is never returned: {all}"
    );

    let first = read("?limit=2").await;
    assert_eq!(started(&first), vec![5, 4]);
    let before = first["next_before"].as_i64().expect("an older page");
    let second = read(&format!("?limit=2&before={before}")).await;
    assert_eq!(started(&second), vec![3, 2]);
    let before = second["next_before"].as_i64().expect("an older page");
    let last = read(&format!("?limit=2&before={before}")).await;
    assert_eq!(started(&last), vec![1]);
    assert_eq!(last["next_before"], Value::Null);

    assert_eq!(started(&read("?phase=refused").await), vec![5, 2]);
    assert_eq!(started(&read("?denied=true").await), vec![5, 4, 2]);
    assert_eq!(started(&read("?phase=end&denied=true").await), vec![4]);

    let (status, _) = call(
        &app,
        "GET",
        "/v1/orgs/acme/plugins/argocd/broker-journal?phase=killed",
        Some(&ada),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an unknown phase");
    let (status, _) = call(
        &app,
        "GET",
        "/v1/orgs/acme/plugins/argocd/broker-journal",
        Some(&mel),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a member reads the journal");
    let (status, _) = call(
        &app,
        "GET",
        "/v1/orgs/acme/plugins/globex-argo/broker-journal",
        Some(&ada),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "another org's installation");
    tdb.cleanup().await;
}

/// A plugin nobody at Scarab wrote, from someone else's repository.
const SLACK_SOURCE: &str =
    "github.com/someone-else/scarab-slack//manifest.yaml@5ac4000000000000000000000000000000000001";
const SLACK_MANIFEST: &str = "\
plugin: slack-notify
capabilities: [use_connection]
connections:
  - name: slack
    auth_schemes: [bearer]
    read_rules:
      - { method: GET, path: /api/conversations.info, query: [channel] }
config_keys: [SLACK_CHANNEL]
";

fn slack_image() -> String {
    format!("ghcr.io/someone-else/slack@sha256:{}", "b".repeat(64))
}

/// Fetch `source` for review in `org`, then install it as `name` from
/// `image` with the hash the fetch returned: (the fetch, the installation).
async fn fetch_and_install(
    app: &axum::Router,
    org: &str,
    name: &str,
    source: &str,
    image: &str,
    config: Value,
) -> (Value, Value) {
    let (status, fetched) = fetch(app, org, source).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    let (status, installation) = call(
        app,
        "POST",
        &format!("/v1/orgs/{org}/plugins"),
        None,
        Some(json!({
            "name": name,
            "image": image,
            "source": source,
            "manifest_sha256": fetched["manifest_sha256"],
            "config": config,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{installation}");
    (fetched, installation)
}

/// A plugin from another publisher, under an image of another repository,
/// installs through the same fetch and install as the Argo CD plugin. The
/// installation records the manifest the file holds, its hash and the
/// image, and the broker and the connections view treat it as they treat
/// Argo CD's.
#[tokio::test]
async fn a_third_party_plugin_installs_through_the_same_path_as_argocd() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let fetcher = Arc::new(argocd_fetcher());
    fetcher.serve(SLACK_SOURCE, SLACK_MANIFEST);
    let apps = apps_with(pg.clone(), fetcher);

    let (fetched, slack) = fetch_and_install(
        &apps.public,
        "acme",
        "slack",
        SLACK_SOURCE,
        &slack_image(),
        json!({ "SLACK_CHANNEL": "deploys" }),
    )
    .await;
    assert_eq!(fetched["source"], json!(SLACK_SOURCE));
    assert_eq!(fetched["content"], json!(SLACK_MANIFEST));
    assert_eq!(fetched["manifest"]["plugin"], json!("slack-notify"));
    assert_eq!(
        fetched["manifest"]["capabilities"],
        json!(["use_connection"])
    );
    assert_eq!(fetched["installed"], json!([]));
    let sha = fetched["manifest_sha256"].as_str().unwrap();

    assert_eq!(slack["plugin"], json!("slack-notify"));
    assert_eq!(slack["principal"], json!("plugin:slack"));
    assert_eq!(slack["image"], json!(slack_image()));
    assert_eq!(slack["manifest_sha256"], json!(sha));
    assert_eq!(slack["manifest"], fetched["manifest"]);
    assert_eq!(slack["config"], json!({ "SLACK_CHANNEL": "deploys" }));

    let row = pg.installation("acme", "slack").await.unwrap().unwrap();
    let file: scarab_plugin::PluginManifest = serde_yaml::from_str(SLACK_MANIFEST).unwrap();
    assert_eq!(row.manifest, file);
    assert_eq!(row.manifest_sha256, sha);
    assert_eq!(row.image, slack_image());
    assert_eq!(row.plugin, "slack-notify");

    let argocd_token = install(&apps.public, "acme", "argocd").await;
    let slack_token = format!("tok-{}", slack["id"].as_str().unwrap());
    for (name, plugin, token, slot) in [
        ("slack", "slack-notify", slack_token, "slack"),
        ("argocd", "argocd", argocd_token, "argocd"),
    ] {
        let (status, me) = whoami(&apps, Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "{name}: {me}");
        assert_eq!(me["name"], json!(name));
        assert_eq!(me["plugin"], json!(plugin));
        let (status, slots) = call(
            &apps.public,
            "GET",
            &format!("/v1/orgs/acme/plugins/{name}/connections"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{name}: {slots}");
        let slots = slots.as_array().unwrap();
        assert_eq!(slots.len(), 1, "{name}: {slots:?}");
        assert_eq!(slots[0]["slot"], json!(slot));
        assert_eq!(slots[0]["configured"], json!(false));
    }
    tdb.cleanup().await;
}

/// The Argo CD plugin gets what an admin approved and nothing else: its
/// installation's manifest is the fetched file's, the install read the
/// source again, and with no source the server can read there is no
/// install at all.
#[tokio::test]
async fn the_argocd_plugin_gets_nothing_an_admin_did_not_approve() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let fetcher = Arc::new(argocd_fetcher());
    let apps = apps_with(pg.clone(), fetcher.clone());

    let (fetched, installation) = fetch_and_install(
        &apps.public,
        "acme",
        "argocd",
        ARGOCD_MANIFEST_SOURCE,
        &image(),
        json!({}),
    )
    .await;
    assert_eq!(fetched["content"], json!(ARGOCD_MANIFEST));
    assert_eq!(installation["manifest"], fetched["manifest"]);
    assert_eq!(installation["manifest_sha256"], fetched["manifest_sha256"]);
    let row = pg.installation("acme", "argocd").await.unwrap().unwrap();
    let file: scarab_plugin::PluginManifest = serde_json::from_str(ARGOCD_MANIFEST).unwrap();
    assert_eq!(row.manifest, file);
    assert_eq!(row.manifest_sha256, manifest_sha256(ARGOCD_MANIFEST));
    // Read once to review and once again at install.
    let asked = fetcher.asked();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(asked[0], asked[1]);

    // A commit the forge does not serve: 400. A host Scarab does not read
    // from anonymously: 403. Neither installs.
    for (source, want) in [
        (
            format!(
                "github.com/thulasi-ram/scarab-ci//crates/scarab-plugin-argocd/manifest.json@{}",
                "e".repeat(40)
            ),
            StatusCode::BAD_REQUEST,
        ),
        (
            format!(
                "forge.example.com/acme/plugins//argocd.json@{}",
                "e".repeat(40)
            ),
            StatusCode::FORBIDDEN,
        ),
    ] {
        let (status, body) = fetch(&apps.public, "acme", &source).await;
        assert_eq!(status, want, "fetch {source}: {body}");
        let (status, body) = call(
            &apps.public,
            "POST",
            "/v1/orgs/acme/plugins",
            None,
            Some(with(argocd("argocd-2"), "source", json!(source))),
        )
        .await;
        assert_eq!(status, want, "install {source}: {body}");
    }
    assert_eq!(
        installed(&apps.public, "acme").await,
        vec!["argocd".to_string()]
    );
    tdb.cleanup().await;
}

/// A fetch lists the org's existing installations of the plugin it read,
/// each with its image and the manifest hash it was approved with; not
/// another plugin's, nor another org's.
#[tokio::test]
async fn a_fetch_lists_the_org_s_installations_of_the_same_plugin() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let fetcher = Arc::new(argocd_fetcher());
    fetcher.serve(SLACK_SOURCE, SLACK_MANIFEST);
    let apps = apps_with(pg, fetcher);
    let (status, fetched) = fetch(&apps.public, "acme", ARGOCD_MANIFEST_SOURCE).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["installed"], json!([]));

    install(&apps.public, "acme", "argocd-stage").await;
    install(&apps.public, "acme", "argocd-prod").await;
    install(&apps.public, "globex", "argocd-prod").await;
    fetch_and_install(
        &apps.public,
        "acme",
        "slack",
        SLACK_SOURCE,
        &slack_image(),
        json!({}),
    )
    .await;

    let (status, fetched) = fetch(&apps.public, "acme", ARGOCD_MANIFEST_SOURCE).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    let mut entries = fetched["installed"].as_array().unwrap().clone();
    entries.sort_by_key(|e| e["name"].as_str().unwrap().to_string());
    let sha = manifest_sha256(ARGOCD_MANIFEST);
    assert_eq!(
        entries,
        vec![
            json!({ "name": "argocd-prod", "image": image(), "manifest_sha256": sha }),
            json!({ "name": "argocd-stage", "image": image(), "manifest_sha256": sha }),
        ]
    );
    tdb.cleanup().await;
}

// ---------------------------------------------------------------------------
// The recorded source, the manifest's image, and upgrades (ADR-0072 §10)
// ---------------------------------------------------------------------------

/// The Argo CD manifest's file at `commit` (40 hex).
fn argocd_at(commit: char) -> String {
    format!(
        "github.com/thulasi-ram/scarab-ci//crates/scarab-plugin-argocd/manifest.json@{}",
        commit.to_string().repeat(40)
    )
}

/// The Argo CD manifest with `edit` applied, as a file.
fn argocd_manifest_with(edit: impl FnOnce(&mut Value)) -> String {
    let mut m: Value = serde_json::from_str(ARGOCD_MANIFEST).unwrap();
    edit(&mut m);
    m.to_string()
}

/// One more read rule than the Argo CD manifest asks for.
fn argocd_wider() -> String {
    argocd_manifest_with(|m| {
        m["connections"][0]["read_rules"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "method": "GET", "path": "/api/v1/account" }))
    })
}

/// The Argo CD manifest without its `/api/version` rule.
fn argocd_narrower() -> String {
    argocd_manifest_with(|m| {
        m["connections"][0]["read_rules"]
            .as_array_mut()
            .unwrap()
            .retain(|r| r["path"] != "/api/version")
    })
}

async fn upgrade_fetch(app: &axum::Router, name: &str, body: Value) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/v1/orgs/acme/plugins/{name}/upgrade-fetch"),
        None,
        Some(body),
    )
    .await
}

async fn upgrade(app: &axum::Router, name: &str, body: Value) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/v1/orgs/acme/plugins/{name}/upgrade"),
        None,
        Some(body),
    )
    .await
}

/// An install records where its manifest was read, normalized, with the
/// connection it was read through and the manifest's hash.
#[tokio::test]
async fn an_install_records_the_manifest_source_and_hash() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg.clone());
    let typed = with(
        argocd("argocd"),
        "source",
        json!(format!("https://{ARGOCD_MANIFEST_SOURCE}")),
    );
    let (status, body) = call(
        &apps.public,
        "POST",
        "/v1/orgs/acme/plugins",
        None,
        Some(typed),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["source"], json!(ARGOCD_MANIFEST_SOURCE));
    assert_eq!(body["source_connection"], Value::Null);
    let row = pg.installation("acme", "argocd").await.unwrap().unwrap();
    assert_eq!(row.source.as_deref(), Some(ARGOCD_MANIFEST_SOURCE));
    assert_eq!(row.source_connection, None);
    assert_eq!(row.manifest_sha256, manifest_sha256(ARGOCD_MANIFEST));
    tdb.cleanup().await;
}

/// The Argo CD manifest names its image repository: an install of an image
/// of another repository is refused, by name, and nothing is stored. A
/// manifest that names no image says so (its `image` is null), and installs
/// from any repository.
#[tokio::test]
async fn a_manifest_that_names_its_image_refuses_an_image_of_another_repository() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let fetcher = Arc::new(argocd_fetcher());
    fetcher.serve(SLACK_SOURCE, SLACK_MANIFEST);
    let apps = apps_with(pg, fetcher);
    let (status, fetched) = fetch(&apps.public, "acme", ARGOCD_MANIFEST_SOURCE).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["manifest"]["image"], json!(REPO));

    for other in [
        format!("ghcr.io/someone-else/argocd-fork@sha256:{}", "b".repeat(64)),
        format!("{REPO}-fork@sha256:{}", "b".repeat(64)),
    ] {
        let (status, body) = call(
            &apps.public,
            "POST",
            "/v1/orgs/acme/plugins",
            None,
            Some(argocd_install_body("fork", &other)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{other}: {body}");
        assert!(body.to_string().contains(REPO), "{body}");
    }
    assert_eq!(installed(&apps.public, "acme").await, Vec::<String>::new());

    let (status, fetched) = fetch(&apps.public, "acme", SLACK_SOURCE).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["manifest"]["image"], Value::Null);
    fetch_and_install(
        &apps.public,
        "acme",
        "slack",
        SLACK_SOURCE,
        &format!("ghcr.io/anyone/anything@sha256:{}", "c".repeat(64)),
        json!({}),
    )
    .await;
    tdb.cleanup().await;
}

/// An upgrade-fetch reads the recorded source's file at the commit the
/// admin names and compares it with the installed manifest: a wider one
/// lists what it adds and needs a step-up, a narrower one lists what it
/// removes and does not. The image, when given, must be one the new
/// manifest names. Nothing is stored.
#[tokio::test]
async fn an_upgrade_fetch_compares_the_new_manifest_with_the_installed_one() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let fetcher = Arc::new(argocd_fetcher());
    fetcher.serve(&argocd_at('b'), &argocd_wider());
    fetcher.serve(&argocd_at('c'), &argocd_narrower());
    let apps = apps_with(pg.clone(), fetcher);
    install(&apps.public, "acme", "argocd").await;

    let (status, wider) =
        upgrade_fetch(&apps.public, "argocd", json!({ "commit": "b".repeat(40) })).await;
    assert_eq!(status, StatusCode::OK, "{wider}");
    assert_eq!(wider["source"], json!(argocd_at('b')));
    assert_eq!(
        wider["current_manifest_sha256"],
        json!(manifest_sha256(ARGOCD_MANIFEST))
    );
    assert_eq!(
        wider["manifest_sha256"],
        json!(manifest_sha256(&argocd_wider()))
    );
    let rules = &wider["diff"]["read_rules"];
    assert_eq!(rules["added"], json!(["argocd: GET /api/v1/account"]));
    assert_eq!(rules["removed"], json!([]));
    assert!(rules["unchanged"]
        .as_array()
        .unwrap()
        .contains(&json!("argocd: GET /api/version")));
    assert_eq!(
        wider["diff"]["capabilities"]["unchanged"],
        json!(["use_connection", "resolve_gates"])
    );
    assert_eq!(wider["diff"]["gates"]["unchanged"], json!(["wait-live"]));
    assert_eq!(wider["needs_step_up"], json!(true));
    assert_eq!(
        wider["step_up_reasons"],
        json!(["adds read rule `argocd: GET /api/v1/account`"])
    );

    // Narrower, on the installed image: no step-up.
    let (status, narrower) = upgrade_fetch(
        &apps.public,
        "argocd",
        json!({ "commit": "c".repeat(40), "image": image() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{narrower}");
    assert_eq!(
        narrower["diff"]["read_rules"]["removed"],
        json!(["argocd: GET /api/version"])
    );
    assert_eq!(narrower["diff"]["read_rules"]["added"], json!([]));
    assert_eq!(narrower["needs_step_up"], json!(false));
    assert_eq!(narrower["step_up_reasons"], json!([]));

    // The same narrower manifest with a new digest: new code, a step-up.
    let new_digest = format!("{REPO}@sha256:{}", "e".repeat(64));
    let (status, new_code) = upgrade_fetch(
        &apps.public,
        "argocd",
        json!({ "commit": "c".repeat(40), "image": new_digest }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{new_code}");
    assert_eq!(new_code["needs_step_up"], json!(true));
    assert_eq!(
        new_code["step_up_reasons"],
        json!([format!(
            "runs new code: image `{new_digest}` instead of `{}`",
            image()
        )])
    );

    // An image the new manifest does not name is refused at the fetch.
    let (status, body) = upgrade_fetch(
        &apps.public,
        "argocd",
        json!({ "commit": "c".repeat(40), "image": format!("ghcr.io/evil/argocd@sha256:{}", "e".repeat(64)) }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Neither a source nor a commit: nothing to read.
    let (status, _) = upgrade_fetch(&apps.public, "argocd", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) =
        upgrade_fetch(&apps.public, "nope", json!({ "commit": "c".repeat(40) })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // An installation from before sources were recorded needs the full source.
    sqlx::query("UPDATE plugin_installations SET manifest_source = NULL")
        .execute(&tdb.pool)
        .await
        .unwrap();
    let (status, body) =
        upgrade_fetch(&apps.public, "argocd", json!({ "commit": "c".repeat(40) })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("no recorded source"), "{body}");
    let (status, body) =
        upgrade_fetch(&apps.public, "argocd", json!({ "source": argocd_at('c') })).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Nothing was stored.
    let row = pg.installation("acme", "argocd").await.unwrap().unwrap();
    assert_eq!(row.manifest_sha256, manifest_sha256(ARGOCD_MANIFEST));
    assert!(pg.installation_versions(&row.id).await.unwrap().is_empty());
    tdb.cleanup().await;
}

/// An upgrade keeps the installation — its id, name, config, storage and
/// state — runs the new image with the new manifest, records the new source,
/// and records the version it replaced.
#[tokio::test]
async fn an_upgrade_keeps_the_installation_and_records_the_version_it_replaced() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let v2_source = format!(
        "github.com/someone-else/scarab-slack//manifest.yaml@{}",
        "2".repeat(40)
    );
    let v2 = SLACK_MANIFEST.replace(
        "config_keys: [SLACK_CHANNEL]",
        "config_keys: [SLACK_CHANNEL, SLACK_EMOJI]",
    );
    assert_ne!(v2, SLACK_MANIFEST);
    let fetcher = Arc::new(argocd_fetcher());
    fetcher.serve(SLACK_SOURCE, SLACK_MANIFEST);
    fetcher.serve(&v2_source, &v2);
    let apps = apps_with(pg.clone(), fetcher);
    let (_, before) = fetch_and_install(
        &apps.public,
        "acme",
        "slack",
        SLACK_SOURCE,
        &slack_image(),
        json!({ "SLACK_CHANNEL": "deploys" }),
    )
    .await;
    let (status, _) = call(
        &apps.public,
        "PUT",
        "/v1/orgs/acme/plugins/slack/storage",
        None,
        Some(json!({ "claim": "slack-state" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, fetched) =
        upgrade_fetch(&apps.public, "slack", json!({ "commit": "2".repeat(40) })).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(
        fetched["diff"]["config_keys"]["added"],
        json!(["SLACK_EMOJI"])
    );
    assert_eq!(fetched["needs_step_up"], json!(false));
    let new_image = format!("ghcr.io/someone-else/slack@sha256:{}", "9".repeat(64));
    let (status, after) = upgrade(
        &apps.public,
        "slack",
        json!({
            "image": new_image,
            "source": fetched["source"],
            "manifest_sha256": fetched["manifest_sha256"],
            "expected_current_sha256": fetched["current_manifest_sha256"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(after["id"], before["id"]);
    assert_eq!(after["name"], json!("slack"));
    assert_eq!(after["image"], json!(new_image));
    assert_eq!(after["manifest_sha256"], fetched["manifest_sha256"]);
    assert_eq!(after["source"], json!(v2_source));
    assert_eq!(after["config"], json!({ "SLACK_CHANNEL": "deploys" }));
    assert_eq!(after["storage"]["claim"], json!("slack-state"));
    assert_eq!(after["state"], json!("enabled"));
    assert_eq!(after["runtime_status"], json!("pending"));
    assert_eq!(
        after["manifest"]["config_keys"],
        json!(["SLACK_CHANNEL", "SLACK_EMOJI"])
    );

    let id = before["id"].as_str().unwrap();
    let history = pg.installation_versions(id).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].image, slack_image());
    assert_eq!(
        history[0].manifest_sha256,
        before["manifest_sha256"].as_str().unwrap()
    );
    assert_eq!(history[0].source.as_deref(), Some(SLACK_SOURCE));
    tdb.cleanup().await;
}

/// The upgrade is a compare-and-swap on the installed manifest: one judged
/// against a manifest that is no longer installed is 409, and so is one
/// whose source no longer reads as the manifest reviewed. Neither writes.
#[tokio::test]
async fn an_upgrade_is_409_when_the_installation_changed_or_the_source_moved() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let fetcher = Arc::new(argocd_fetcher());
    fetcher.serve(&argocd_at('c'), &argocd_narrower());
    let apps = apps_with(pg.clone(), fetcher.clone());
    install(&apps.public, "acme", "argocd").await;
    let (status, fetched) =
        upgrade_fetch(&apps.public, "argocd", json!({ "commit": "c".repeat(40) })).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    let body = json!({
        "image": format!("{REPO}@sha256:{}", "e".repeat(64)),
        "source": fetched["source"],
        "manifest_sha256": fetched["manifest_sha256"],
        "expected_current_sha256": fetched["current_manifest_sha256"],
    });

    // Someone else's change landed first.
    let (status, msg) = upgrade(
        &apps.public,
        "argocd",
        with(
            body.clone(),
            "expected_current_sha256",
            json!("0".repeat(64)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{msg}");
    // The source now reads as another manifest.
    fetcher.serve(&argocd_at('c'), &argocd_wider());
    let (status, msg) = upgrade(&apps.public, "argocd", body.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{msg}");
    assert!(msg.to_string().contains("you reviewed"), "{msg}");
    let row = pg.installation("acme", "argocd").await.unwrap().unwrap();
    assert_eq!(row.manifest_sha256, manifest_sha256(ARGOCD_MANIFEST));
    assert_eq!(row.image, image());
    assert!(pg.installation_versions(&row.id).await.unwrap().is_empty());

    // As reviewed, it goes through; the same body again is judged against a
    // manifest no longer installed.
    fetcher.serve(&argocd_at('c'), &argocd_narrower());
    let (status, msg) = upgrade(&apps.public, "argocd", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{msg}");
    let (status, msg) = upgrade(&apps.public, "argocd", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{msg}");
    tdb.cleanup().await;
}

/// While plugins are off an upgrade is refused (409), as an install is, and
/// nothing is written.
#[tokio::test]
async fn an_upgrade_is_refused_while_plugins_are_off() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let fetcher = Arc::new(argocd_fetcher());
    fetcher.serve(&argocd_at('c'), &argocd_narrower());
    let apps = apps_with(pg.clone(), fetcher.clone());
    install(&apps.public, "acme", "argocd").await;
    let (status, fetched) =
        upgrade_fetch(&apps.public, "argocd", json!({ "commit": "c".repeat(40) })).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    let off = router(state_with(pg.clone(), false, fetcher));
    let (status, msg) = upgrade(
        &off,
        "argocd",
        json!({
            "image": image(),
            "source": fetched["source"],
            "manifest_sha256": fetched["manifest_sha256"],
            "expected_current_sha256": fetched["current_manifest_sha256"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{msg}");
    assert!(msg.to_string().contains("SCARAB_PLUGINS=off"), "{msg}");
    let row = pg.installation("acme", "argocd").await.unwrap().unwrap();
    assert_eq!(row.manifest_sha256, manifest_sha256(ARGOCD_MANIFEST));
    assert!(pg.installation_versions(&row.id).await.unwrap().is_empty());
    tdb.cleanup().await;
}
