//! Plugins end to end over HTTP (ADR-0072, MMP slice d1): an org admin
//! installs the first-party plugin on the public API, and the plugin proves
//! who it is to the CredentialBroker's own router (`whoami`) — refused when
//! its token maps to nothing, and from the very next request once killed.
//! Report ingest went with the Deployment record (ADR-0074).
//!
//! Real Postgres for every store; the token verifier is the one fake (it
//! stands in for TokenReview, a true external — slice d2) and the clock is a
//! `FakeClock`. Each test gets its own database (`fresh_db`) and skips
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
use scarab_plugin::{PluginTokenVerifier, TokenRejected, VerifiedServiceAccount};
use scarab_server::plugins::PluginPolicy;
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
        argocd_repo: REPO.into(),
        dev_allow_tag: false,
    }
}

fn state(pg: Arc<PostgresDb>, enabled: bool) -> AppState {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    AppState::new(db, Arc::new(FakeClock::new(CLOCK_START_MS)), logs)
        .with_org_environments(pg.clone())
        .with_plugins(pg, policy(enabled))
        .with_plugin_tokens(Arc::new(FakeTokenReview))
}

/// The public API and the broker over one state, authz off (every caller an
/// owner).
struct Apps {
    public: axum::Router,
    broker: axum::Router,
}

fn apps(pg: Arc<PostgresDb>) -> Apps {
    let st = state(pg, true);
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

/// Install `name` and return its token.
async fn install(app: &axum::Router, org: &str, name: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/v1/orgs/{org}/plugins"),
        None,
        Some(json!({ "name": name, "plugin": "argocd", "image": image() })),
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

#[tokio::test]
async fn only_a_first_party_plugin_pinned_by_digest_installs() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg.clone());
    for (plugin, image) in [
        (
            "flux",
            format!("ghcr.io/fluxcd/flux-plugin@sha256:{}", "a".repeat(64)),
        ),
        ("argocd", format!("{REPO}:latest")),
        (
            "argocd",
            format!("ghcr.io/evil/argocd@sha256:{}", "a".repeat(64)),
        ),
    ] {
        let (status, body) = call(
            &apps.public,
            "POST",
            "/v1/orgs/acme/plugins",
            None,
            Some(json!({ "name": "p", "plugin": plugin, "image": image })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{plugin} {image}");
        assert!(
            body.as_str()
                .unwrap()
                .contains("third-party installs are off (ADR-0072)"),
            "{body}"
        );
    }
    let (_, list) = call(&apps.public, "GET", "/v1/orgs/acme/plugins", None, None).await;
    assert_eq!(list, json!([]));

    // With SCARAB_PLUGINS=off even a first-party install is refused.
    let off = router(state(pg, false));
    let (status, _) = call(
        &off,
        "POST",
        "/v1/orgs/acme/plugins",
        None,
        Some(json!({ "name": "argocd", "plugin": "argocd", "image": image() })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
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
            Some(json!({ "name": "argocd", "plugin": "argocd", "image": image() })),
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
        Some(json!({ "name": "argocd", "plugin": "argocd", "image": image() })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "the name is retired");
    tdb.cleanup().await;
}

/// Installing, listing and killing need Administer on the Org.
#[tokio::test]
async fn a_member_cannot_install_list_or_kill() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let scoped = |subject: &str| Principal {
        subject: subject.into(),
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
    let body = || Some(json!({ "name": "argocd", "plugin": "argocd", "image": image() }));

    let (status, _) = call(&app, "POST", "/v1/orgs/acme/plugins", Some(&mel), body()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a member installs");
    for uri in ["/v1/orgs/acme/plugins", "/v1/orgs/acme/plugin-catalog"] {
        let (status, _) = call(&app, "GET", uri, Some(&mel), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "a member reads {uri}");
    }
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
        Some(json!({ "name": "argocd-prod", "plugin": "argocd", "image": image() })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], json!("step_up_required"));
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
