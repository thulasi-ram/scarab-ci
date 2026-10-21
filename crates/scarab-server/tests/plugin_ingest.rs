//! Plugins end to end over HTTP (ADR-0072, MMP slice d1): an org admin
//! installs the first-party plugin on the public API, an Environment lists
//! its source, and the plugin reports through the CredentialBroker's own
//! router — onto the Environment board, under the installation's name.
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

/// Not ahead of the database clock: the Deployment store refuses a runaway
/// `received_at`.
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
        .with_deployments(pg.clone())
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

async fn list_source(app: &axum::Router, org: &str, env: &str, installation: &str, grouping: &str) {
    let (status, body) = call(
        app,
        "PUT",
        &format!("/v1/orgs/{org}/environments/{env}"),
        None,
        Some(json!({
            "deployment_sources": [{ "installation": installation, "vendor_grouping": grouping }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

fn status_item(key: &str, sequence: u64, version: &str) -> Value {
    json!({
        "type": "status",
        "idempotency_key": key,
        "sequence": sequence,
        "vendor_grouping": "prod",
        "target_id": "argocd/web",
        "version": version,
        "version_scheme": "argocd.target-images.v1",
        "status": "live",
        "label": "Synced, Healthy",
        "link": "https://argo.example/applications/argocd/web",
        "source_at": 42,
        "owners": ["acme/web"]
    })
}

async fn report(apps: &Apps, token: &str, items: Vec<Value>) -> (StatusCode, Value) {
    call(
        &apps.broker,
        "POST",
        "/_scarab/plugin/v1/reports",
        Some(token),
        Some(json!({ "items": items })),
    )
    .await
}

#[tokio::test]
async fn a_report_lands_on_the_board_under_the_installations_name() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg);
    let token = install(&apps.public, "acme", "argocd-prod").await;
    list_source(&apps.public, "acme", "prod", "argocd-prod", "prod").await;

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
    assert_eq!(whoami["org"], json!("acme"));

    let (status, results) = report(&apps, &token, vec![status_item("k1", 1, "v1")]).await;
    assert_eq!(status, StatusCode::OK, "{results}");
    assert_eq!(results["results"][0]["outcome"], json!("accepted"));
    assert_eq!(results["results"][0]["input_seq"], json!(1));

    let (status, board) = call(&apps.public, "GET", "/v1/orgs/acme/board", None, None).await;
    assert_eq!(status, StatusCode::OK, "{board}");
    let row = &board[0];
    assert_eq!(
        row["target"],
        json!({ "environment": "prod", "source": "argocd-prod", "id": "argocd/web" }),
        "core set the source from the installation"
    );
    assert_eq!(row["live"]["version"], json!("v1"));
    assert_eq!(
        row["live"]["unexplained"],
        json!(true),
        "no Run asked for it"
    );
    let reported = &row["live"]["reports"][0];
    assert_eq!(
        reported["at"],
        json!(CLOCK_START_MS),
        "core's clock orders it"
    );
    assert_eq!(reported["source_at"], json!(42), "the source's, shown");

    // The plugin can reseed from its cursors; the Environment shows owners.
    let (_, targets) = call(
        &apps.broker,
        "GET",
        "/_scarab/plugin/v1/targets",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(targets["targets"][0]["last_sequence"], json!(1));
    let (status, owned) = call(
        &apps.public,
        "GET",
        "/v1/orgs/acme/environments/prod/deployment-targets",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{owned}");
    assert_eq!(owned[0]["owner_projects"], json!(["acme/web"]));
    assert_eq!(owned[0]["principal"], json!("plugin:argocd-prod"));

    tdb.cleanup().await;
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
        let (status, _) = report(
            &apps,
            bearer.as_deref().unwrap_or(""),
            vec![status_item("k1", 1, "v1")],
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer:?}");
    }
    let (status, _) = report(&apps, &token, vec![]).await;
    assert_eq!(status, StatusCode::OK, "the real token gets in");
    tdb.cleanup().await;
}

#[tokio::test]
async fn a_kill_refuses_the_very_next_request_and_enable_restores_it() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg);
    let token = install(&apps.public, "acme", "argocd").await;
    list_source(&apps.public, "acme", "prod", "argocd", "prod").await;
    let (status, _) = report(&apps, &token, vec![status_item("k1", 1, "v1")]).await;
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
    let (status, body) = report(&apps, &token, vec![status_item("k2", 2, "v2")]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = call(
        &apps.broker,
        "GET",
        "/_scarab/plugin/v1/whoami",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = call(
        &apps.public,
        "POST",
        "/v1/orgs/acme/plugins/argocd/enable",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, results) = report(&apps, &token, vec![status_item("k2", 2, "v2")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(results["results"][0]["outcome"], json!("accepted"));
    tdb.cleanup().await;
}

#[tokio::test]
async fn a_source_in_the_body_is_400_and_nothing_is_recorded() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg);
    let token = install(&apps.public, "acme", "argocd").await;
    list_source(&apps.public, "acme", "prod", "argocd", "prod").await;
    let mut claims_other = status_item("k1", 1, "v1");
    claims_other["source"] = json!("argocd-other-team");
    let (status, body) = report(
        &apps,
        &token,
        vec![status_item("k0", 1, "v0"), claims_other],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.as_str().unwrap().contains("`source`"), "{body}");
    let (_, board) = call(&apps.public, "GET", "/v1/orgs/acme/board", None, None).await;
    assert_eq!(board, json!([]), "the whole request was refused");
    tdb.cleanup().await;
}

/// A value-rule failure rejects only its item; a key reused for other
/// content refuses the whole batch (409).
#[tokio::test]
async fn bad_items_are_rejected_alone_and_a_reused_key_is_409() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg);
    let token = install(&apps.public, "acme", "argocd").await;
    list_source(&apps.public, "acme", "prod", "argocd", "prod").await;
    let mut script_link = status_item("k1", 1, "v1");
    script_link["link"] = json!("javascript:alert(1)");
    let mut other_org = status_item("k2", 2, "v2");
    other_org["owners"] = json!(["initech/web"]);
    let (status, results) = report(
        &apps,
        &token,
        vec![script_link, other_org, status_item("k3", 3, "v3")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{results}");
    let outcomes: Vec<&str> = results["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, vec!["rejected", "rejected", "accepted"]);
    assert!(results["results"][0]["reason"].is_string());

    let (status, _) = report(&apps, &token, vec![status_item("k3", 4, "v4")]).await;
    assert_eq!(status, StatusCode::CONFLICT);
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

/// F1 (c-min audit): an Environment source must name an installation in the
/// same org; an uninstall takes its sources and retires its name.
#[tokio::test]
async fn sources_must_name_an_installation_in_the_org_and_uninstall_takes_them() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let apps = apps(pg);
    let put = |installation: &'static str| {
        let app = apps.public.clone();
        async move {
            call(
                &app,
                "PUT",
                "/v1/orgs/acme/environments/prod",
                None,
                Some(json!({
                    "deployment_sources": [{ "installation": installation, "vendor_grouping": "prod" }]
                })),
            )
            .await
        }
    };
    let (status, body) = put("argocd").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    install(&apps.public, "initech", "argocd").await;
    let (status, _) = put("argocd").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "another org's installation is not this org's"
    );
    install(&apps.public, "acme", "argocd").await;
    let (status, _) = put("argocd").await;
    assert_eq!(status, StatusCode::OK);

    let (status, uninstalled) = call(
        &apps.public,
        "DELETE",
        "/v1/orgs/acme/plugins/argocd",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{uninstalled}");
    assert_eq!(
        uninstalled["removed_deployment_sources"],
        json!([{ "environment": "prod", "vendor_grouping": "prod" }])
    );
    let (_, prod) = call(
        &apps.public,
        "GET",
        "/v1/orgs/acme/environments/prod",
        None,
        None,
    )
    .await;
    assert_eq!(prod["deployment_sources"], json!([]));
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

/// Installing, killing and uninstalling need Administer on the Org; an
/// Environment's DeploymentTargets need only Read.
#[tokio::test]
async fn a_member_cannot_install_but_can_read_deployment_targets() {
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
    let (status, _) = call(
        &app,
        "GET",
        "/v1/orgs/acme/environments/prod/deployment-targets",
        Some(&mel),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a member reads the targets");

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
