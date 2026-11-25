//! Check a leaked value (`POST /v1/orgs/{org}/secrets/match`): an org Admin
//! pastes text found outside Scarab and learns which of the org's current
//! values occur in it — names and scopes only.
//!
//! Classical, per ADR-0017: a real Postgres for the event log, the RBAC
//! bindings, the org Environments and the connection and plugin registries
//! (skipped without `SCARAB_TEST_DATABASE_URL`); the secret store is the
//! in-memory `FakeSecrets`; the one fake is the authenticator (a software
//! passkey). Canaries: what is pasted holds `PASTE-CANARY-…`, stored values
//! are `VALUE-CANARY-…`; neither may come back in an answer or an event.

mod common;
mod step_up_harness;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{json, Value};
use tower::ServiceExt;

use common::fresh_pg;
use scarab_db_postgres::PostgresDb;
use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_identity::{api_token_hash, ApiToken, ApiTokenStore, Role, Scope};
use scarab_plugin::{
    ConnectionSlot, InstallationState, PluginCapability, PluginConnection,
    PluginConnectionAuthScheme, PluginInstallation, PluginManifest, PluginStore, RequestRule,
    RuntimeStatus,
};
use scarab_project::{OrgEnvironment, OrgEnvironmentStore};
use scarab_secrets::SecretScope;
use scarab_testkit::FakeSecrets;
use scarab_webauthn::soft::SoftPasskey;

use step_up_harness::*;

const KIND: &str = "secret.match_checked";
const HOUR: i64 = 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An enforcing harness on `pg`, with org Environments, the forge
/// connection registry and the plugin registry on it, and `secrets`.
fn world(pg: &Arc<PostgresDb>, secrets: &Arc<FakeSecrets>) -> Harness {
    let (pg, secrets) = (pg.clone(), secrets.clone());
    harness_on(Backend::Pg(pg.clone()), move |st| {
        st.with_secrets(secrets)
            .with_org_environments(pg.clone())
            .with_forge_connections(pg.clone())
            .with_plugins(pg, scarab_server::plugins::PluginPolicy::off())
    })
}

fn org_scope(org: &str) -> SecretScope {
    SecretScope::Org { org: org.into() }
}

fn org_env(org: &str, environment: &str) -> SecretScope {
    SecretScope::OrgEnvironment {
        org: org.into(),
        environment: environment.into(),
    }
}

fn repo(org: &str, repo: &str) -> SecretScope {
    SecretScope::Repo {
        org: org.into(),
        repo: repo.into(),
    }
}

fn repo_env(org: &str, project: &str, environment: &str) -> SecretScope {
    SecretScope::EnvironmentProject {
        org: org.into(),
        project: project.into(),
        environment: environment.into(),
    }
}

async fn put(secrets: &FakeSecrets, scope: SecretScope, key: &str, value: &str) {
    scarab_secrets::SecretProvider::put(
        secrets,
        &scope,
        scarab_secrets::Secret {
            key: key.into(),
            value: value.as_bytes().to_vec(),
        },
    )
    .await
    .unwrap();
}

async fn environment(pg: &PostgresDb, org: &str, name: &str) {
    pg.put_org_environment(&OrgEnvironment {
        org: org.into(),
        name: name.into(),
        display_name: None,
        description: String::new(),
        protection: Default::default(),
        eligibility: Default::default(),
        created_at: 0,
        updated_at: 0,
        updated_by: "test".into(),
    })
    .await
    .unwrap();
}

/// A forge connection serving `org`, its credential stored as `value`.
async fn forge(pg: &PostgresDb, secrets: &FakeSecrets, org: &str, value: &str) -> String {
    let id = format!("forgejo-{org}");
    ForgeConnectionStore::put_connection(
        pg,
        &ForgeConnection {
            id: id.clone(),
            kind: ForgeKind::Forgejo,
            base_url: format!("https://git.{org}.test"),
            credential_ref: format!("cred-{org}"),
            org: Some(org.into()),
            installation_id: None,
            account_id: None,
            account_login: None,
        },
    )
    .await
    .unwrap();
    put(
        secrets,
        org_scope(scarab_server::FORGE_CREDENTIALS_ORG),
        &format!("cred-{org}"),
        value,
    )
    .await;
    id
}

/// A plugin installed in `org` with its one connection bound, the
/// credential stored as `value`. Returns the installation id.
async fn plugin(pg: &PostgresDb, secrets: &FakeSecrets, org: &str, id: &str, value: &str) {
    PluginStore::install(
        pg,
        &PluginInstallation {
            id: id.into(),
            org: org.into(),
            name: "argocd".into(),
            plugin: "argocd".into(),
            image: format!("ghcr.io/acme/argocd@sha256:{}", "a".repeat(64)),
            manifest: PluginManifest {
                plugin: "argocd".into(),
                image: None,
                capabilities: vec![PluginCapability::UseConnection],
                connections: vec![ConnectionSlot {
                    name: "argocd".into(),
                    auth_schemes: vec!["bearer".into()],
                    read_rules: vec![RequestRule {
                        method: "GET".into(),
                        path: "/api/v1/applications".into(),
                        query: vec![],
                    }],
                }],
                config_keys: vec![],
                gates: Default::default(),
            },
            manifest_sha256: "0".repeat(64),
            source: None,
            source_connection: None,
            config: BTreeMap::new(),
            state: InstallationState::Enabled,
            state_version: 0,
            runtime_status: RuntimeStatus::Pending,
            runtime_message: String::new(),
            probe_passed_at: None,
            approved_by: "amy".into(),
            approved_at: 1_000,
            killed_by: None,
            killed_at: None,
            storage: None,
        },
    )
    .await
    .unwrap();
    let credential_ref = format!("pc-{id}");
    PluginStore::put_connection(
        pg,
        &PluginConnection {
            installation_id: id.into(),
            slot: "argocd".into(),
            base_url: format!("https://argo.{org}.test"),
            auth_scheme: PluginConnectionAuthScheme::Bearer,
            credential_ref: credential_ref.clone(),
            ca_pem: None,
            updated_by: "amy".into(),
            updated_at: 2_000,
        },
    )
    .await
    .unwrap();
    put(
        secrets,
        scarab_server::plugins::plugin_connection_secret_scope(),
        &credential_ref,
        value,
    )
    .await;
}

/// `subject` (no global role) in a browser with a passkey, `role` in `org`.
async fn member(h: &Harness, subject: &str, org: &str, role: Role) -> (Sess, SoftPasskey) {
    h.grant(subject, Scope::Org(org.into()), role).await;
    let s = h.browser_session(scoped(subject)).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, &format!("{subject}'s key")).await;
    (s, soft)
}

/// An org Admin of `org` with an open step-up window.
async fn admin(h: &Harness, subject: &str, org: &str) -> (Sess, SoftPasskey) {
    let (s, mut soft) = member(h, subject, org, Role::Admin).await;
    h.open_window(&s, &mut soft).await;
    (s, soft)
}

async fn check(h: &Harness, s: &Sess, org: &str, text: &str) -> (StatusCode, Value) {
    h.cookie(
        "POST",
        &format!("/v1/orgs/{org}/secrets/match"),
        Some(json!({ "text": text })),
        s,
    )
    .await
}

/// The match whose `key` (a secret) or `connection` kind (a connection) is
/// `name`, with `scope` for a secret.
fn find<'a>(body: &'a Value, f: impl Fn(&Value) -> bool) -> &'a Value {
    body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| f(m))
        .unwrap_or_else(|| panic!("no such match in {body}"))
}

/// An API token of amy's; returns its plaintext.
async fn pat(h: &Harness, id: &str, scope: Scope, role: Role) -> String {
    let plaintext = format!("scarab_pat_{id}_0123456789abcdef0123456789abcdef");
    let now = h.now().await;
    h.tokens
        .put(
            &ApiToken {
                id: id.into(),
                name: format!("ci-{id}"),
                owner_subject: "amy".into(),
                scope,
                role,
                expires_at: now + 30 * 24 * HOUR,
                created_by: "amy".into(),
                created_at: now,
                last_used_at: None,
                use_count: 0,
                revoked_at: None,
            },
            &api_token_hash(&plaintext),
        )
        .await
        .unwrap();
    plaintext
}

/// Every event in the log, serialized.
async fn log_text(h: &Harness) -> String {
    let all = h
        .log
        .list(&scarab_identity::SecurityEventFilter {
            limit: 10_000,
            ..Default::default()
        })
        .await
        .unwrap();
    format!("{all:?}")
}

// ---------------------------------------------------------------------------
// B. What is searched, and how it is named
// ---------------------------------------------------------------------------

/// One value at each of the four secret scopes, a forge credential and a
/// plugin credential, all in one paste: each is named by kind and scope.
#[tokio::test]
async fn every_scope_is_searched_and_named() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    environment(&pg, "acme", "production").await;
    put(
        &secrets,
        org_scope("acme"),
        "ORG_TOKEN",
        "VALUE-CANARY-org-0000001",
    )
    .await;
    put(
        &secrets,
        org_env("acme", "production"),
        "DEPLOY_TOKEN",
        "VALUE-CANARY-orgenv-00001",
    )
    .await;
    put(
        &secrets,
        repo("acme", "web"),
        "WEB_TOKEN",
        "VALUE-CANARY-repo-web-001",
    )
    .await;
    put(
        &secrets,
        repo_env("acme", "web", "production"),
        "DEPLOY_TOKEN",
        "VALUE-CANARY-web-prod-001",
    )
    .await;
    put(
        &secrets,
        repo("acme", "api"),
        "UNUSED",
        "VALUE-CANARY-unused-00001",
    )
    .await;
    let forge_id = forge(&pg, &secrets, "acme", "VALUE-CANARY-forge-000001").await;
    plugin(
        &pg,
        &secrets,
        "acme",
        "0000000001",
        "VALUE-CANARY-plugin-00001",
    )
    .await;
    let h = world(&pg, &secrets);
    let (s, _) = admin(&h, "dan", "acme").await;

    let paste = "PASTE-CANARY-env\n\
                 ORG=VALUE-CANARY-org-0000001\n\
                 OE=VALUE-CANARY-orgenv-00001\n\
                 WEB=VALUE-CANARY-repo-web-001\n\
                 WP=VALUE-CANARY-web-prod-001\n\
                 F=VALUE-CANARY-forge-000001\n\
                 P=VALUE-CANARY-plugin-00001\n";
    let (st, body) = check(&h, &s, "acme", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["matches"].as_array().unwrap().len(), 6, "{body}");
    assert_eq!(
        body["searched"],
        json!({ "secrets": 5, "repositories": 2, "connections": 2 })
    );

    let org = find(&body, |m| m["key"] == "ORG_TOKEN");
    assert_eq!(org["kind"], "secret");
    assert_eq!(org["scope"], "org");
    assert!(org.get("repo").is_none() && org.get("environment").is_none());
    assert_eq!(org["in_use"], true);

    let oe = find(&body, |m| m["scope"] == "org_environment");
    assert_eq!(oe["key"], "DEPLOY_TOKEN");
    assert_eq!(oe["environment"], "production");
    assert!(oe.get("repo").is_none());
    // web sets its own for production; api gets the org's.
    assert_eq!(oe["overridden_in"], json!(["web · production"]));
    assert_eq!(oe["in_use"], true);

    let web = find(&body, |m| m["scope"] == "repo");
    assert_eq!(web["key"], "WEB_TOKEN");
    assert_eq!(web["repo"], "web");
    assert!(web.get("environment").is_none());

    let wp = find(&body, |m| m["scope"] == "repo_environment");
    assert_eq!(wp["key"], "DEPLOY_TOKEN");
    assert_eq!(wp["repo"], "web");
    assert_eq!(wp["environment"], "production");
    assert_eq!(wp["overridden_in"], json!([]));
    assert_eq!(wp["in_use"], true);

    let f = find(&body, |m| m["connection"] == "forge");
    assert_eq!(f["kind"], "connection");
    assert_eq!(f["id"], forge_id);
    assert_eq!(f["name"], "git.acme.test");
    assert_eq!(f["base_url"], "https://git.acme.test");
    assert!(f.get("slot").is_none());

    let p = find(&body, |m| m["connection"] == "plugin");
    assert_eq!(p["id"], "0000000001");
    assert_eq!(p["name"], "argocd");
    assert_eq!(p["slot"], "argocd");
    assert_eq!(p["base_url"], "https://argo.acme.test");

    tdb.cleanup().await;
}

/// An org value that every repository overrides for the org's only
/// Environment is still reported, with who stands in front of it, and is
/// not in use; once a repository with no value of its own is enabled, it is.
#[tokio::test]
async fn an_overridden_value_is_still_reported_with_who_overrides_it() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    environment(&pg, "acme", "production").await;
    put(
        &secrets,
        org_scope("acme"),
        "SHARED",
        "VALUE-CANARY-shared-0001",
    )
    .await;
    put(
        &secrets,
        repo_env("acme", "web", "production"),
        "SHARED",
        "web-own",
    )
    .await;
    put(
        &secrets,
        repo_env("acme", "api", "production"),
        "SHARED",
        "api-own",
    )
    .await;
    let h = world(&pg, &secrets);
    let (s, _) = admin(&h, "dan", "acme").await;
    let paste = "leaked: VALUE-CANARY-shared-0001";

    let (st, body) = check(&h, &s, "acme", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let m = find(&body, |m| m["key"] == "SHARED");
    assert_eq!(m["scope"], "org");
    assert_eq!(m["in_use"], false, "{body}");
    assert_eq!(
        m["overridden_in"],
        json!(["api · production", "web · production"])
    );

    // `docs` is enabled through a connection and sets nothing of its own.
    let conn = forge(&pg, &secrets, "acme", "VALUE-CANARY-forge-xxxxxx").await;
    pg.bind_repo(
        &conn,
        &RepoRef {
            owner: "acme".into(),
            name: "docs".into(),
        },
        "acme",
        "docs",
    )
    .await
    .unwrap();
    let (st, body) = check(&h, &s, "acme", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let m = find(&body, |m| m["key"] == "SHARED");
    assert_eq!(m["in_use"], true, "{body}");
    assert_eq!(body["searched"]["repositories"], 3);

    tdb.cleanup().await;
}

/// Another org holding the same value, and connections of its own, never
/// matches: a check reads only this org's scopes and connections.
#[tokio::test]
async fn another_orgs_values_and_connections_never_match() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    put(&secrets, org_scope("acme"), "OWN", "acme-own-value").await;
    put(
        &secrets,
        org_scope("globex"),
        "SHARED",
        "VALUE-CANARY-shared-0001",
    )
    .await;
    put(
        &secrets,
        repo("globex", "web"),
        "WEB",
        "VALUE-CANARY-globex-web-1",
    )
    .await;
    forge(&pg, &secrets, "globex", "VALUE-CANARY-forge-globex").await;
    plugin(
        &pg,
        &secrets,
        "globex",
        "0000000009",
        "VALUE-CANARY-plugin-globx",
    )
    .await;
    let h = world(&pg, &secrets);
    let (s, _) = admin(&h, "dan", "acme").await;
    h.grant("dan", Scope::Org("globex".into()), Role::Admin)
        .await;
    let paste = "VALUE-CANARY-shared-0001 VALUE-CANARY-globex-web-1 \
                 VALUE-CANARY-forge-globex VALUE-CANARY-plugin-globx";

    let (st, body) = check(&h, &s, "acme", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["matches"], json!([]), "{body}");
    assert_eq!(
        body["searched"],
        json!({ "secrets": 1, "repositories": 0, "connections": 0 })
    );

    // The same paste in globex finds all four: the fixture is live.
    let (st, body) = check(&h, &s, "globex", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["matches"].as_array().unwrap().len(), 4, "{body}");

    tdb.cleanup().await;
}

/// Only an Admin of the org may check: a repository's Admin and an org
/// Member are refused, and a reserved org is refused even to an Owner.
/// Nothing runs and nothing is recorded.
#[tokio::test]
async fn only_an_org_admin_may_check() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    put(&secrets, org_scope("acme"), "K", "VALUE-CANARY-org-0000001").await;
    let h = world(&pg, &secrets);
    let paste = "VALUE-CANARY-org-0000001";

    h.grant(
        "rita",
        Scope::Project {
            org: "acme".into(),
            name: "web".into(),
        },
        Role::Admin,
    )
    .await;
    let rita = h.browser_session(scoped("rita")).await;
    let (st, body) = check(&h, &rita, "acme", paste).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_ne!(body["code"], "step_up_required");

    let (mia, _) = member(&h, "mia", "acme", Role::Member).await;
    let (st, body) = check(&h, &mia, "acme", paste).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_ne!(body["code"], "step_up_required");

    // amy is a global Owner.
    let amy = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&amy, &mut soft, "Laptop").await;
    h.open_window(&amy, &mut soft).await;
    for reserved in ["_forge", "_plugin_connections"] {
        let (st, body) = check(&h, &amy, reserved, paste).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{reserved}: {body}");
    }

    assert!(h.events(KIND).await.is_empty(), "no check ran");
    tdb.cleanup().await;
}

/// Blank text is a 400; text over 4096 bytes (bytes, not characters) is a
/// 413; 4096 bytes is accepted.
#[tokio::test]
async fn blank_and_oversized_text_is_refused() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    let h = world(&pg, &secrets);
    let (s, _) = admin(&h, "dan", "acme").await;

    for blank in ["", "   ", "\n\t \r\n"] {
        let (st, body) = check(&h, &s, "acme", blank).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "empty");
    }
    let (st, body) = check(&h, &s, "acme", &"a".repeat(4097)).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["code"], "too_long");
    assert_eq!(body["max_bytes"], 4096);
    let (st, body) = check(&h, &s, "acme", &"a".repeat(4096)).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // 2049 two-byte characters: 4098 bytes.
    let (st, body) = check(&h, &s, "acme", &"é".repeat(2049)).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    let (st, body) = check(&h, &s, "acme", &"é".repeat(2048)).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // Not `{"text": ...}`.
    let (st, body) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/secrets/match",
            Some(json!({ "value": "PASTE-CANARY-x" })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "bad_body");
    assert!(!body.to_string().contains("PASTE-CANARY"));

    // Only the two checks that ran are recorded.
    assert_eq!(h.events(KIND).await.len(), 2);
    tdb.cleanup().await;
}

// ---------------------------------------------------------------------------
// C. Step-up, the record, the hourly limit
// ---------------------------------------------------------------------------

/// Every check needs a step-up. Neither the refusal, nor a machine's parked
/// request (its summary, its stored row), nor the event log shows the text;
/// once confirmed, the check answers.
#[tokio::test]
async fn a_check_needs_step_up_and_the_parked_request_never_shows_the_text() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    put(&secrets, org_scope("acme"), "K", "VALUE-CANARY-org-0000001").await;
    let h = world(&pg, &secrets);
    let paste = "PASTE-CANARY-1 VALUE-CANARY-org-0000001";

    // A browser with no window: asked, and the refusal does not echo.
    let (s, mut soft) = member(&h, "dan", "acme", Role::Admin).await;
    let (st, body) = check(&h, &s, "acme", paste).await;
    assert_step_up(st, &body, "window");
    assert_eq!(body["op"], "secret.match");
    assert_eq!(body["summary"]["target"], "acme");
    assert_eq!(body["summary"]["changes"], json!([]));
    assert!(!body.to_string().contains("CANARY"), "{body}");
    let (st, conf) = h.answer(&s, &mut soft, &body["challenge"]).await;
    assert_eq!(st, StatusCode::OK, "{conf}");
    let (st, body) = check(&h, &s, "acme", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["matches"].as_array().unwrap().len(), 1);

    // An API token parks a request for its human.
    h.grant("amy", Scope::Org("acme".into()), Role::Admin).await;
    let amy = h.browser_session(scoped("amy")).await;
    let mut amy_key = passkey();
    h.enroll(&amy, &mut amy_key, "Laptop").await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let (st, raised) = h
        .bearer(
            "POST",
            "/v1/orgs/acme/secrets/match",
            Some(json!({ "text": paste })),
            &token,
        )
        .await;
    assert_step_up(st, &raised, "window");
    assert!(!raised.to_string().contains("CANARY"), "{raised}");
    let id = raised["request_id"]
        .as_str()
        .expect("a request")
        .to_string();

    let row = h.requests.get_request(&id).await.unwrap().expect("stored");
    assert!(
        !row.summary.to_string().contains("CANARY"),
        "{}",
        row.summary
    );
    let sealed = row.sealed.clone().expect("sealed while pending");
    assert!(!String::from_utf8_lossy(&sealed).contains("CANARY"));
    let (st, seen) = h
        .cookie("GET", &format!("/v1/step-up/requests/{id}"), None, &amy)
        .await;
    assert_eq!(st, StatusCode::OK, "{seen}");
    assert_eq!(seen["op"], "secret.match");
    assert_eq!(seen["summary"]["target"], "acme");
    assert!(!seen.to_string().contains("CANARY"), "{seen}");
    let (st, summary) = h
        .cookie("GET", "/v1/orgs/acme/security/summary", None, &amy)
        .await;
    assert_eq!(st, StatusCode::OK, "{summary}");
    assert!(!summary.to_string().contains("CANARY"));

    // amy confirms; the check runs once and the answer goes to her.
    let (st, ch) = h
        .cookie(
            "POST",
            &format!("/v1/step-up/requests/{id}/options"),
            None,
            &amy,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{ch}");
    let cred = amy_key.assert(&ch["public_key"], ORIGIN).unwrap();
    let (st, done) = h
        .cookie(
            "POST",
            &format!("/v1/step-up/requests/{id}/confirm"),
            Some(json!({ "challenge_id": ch["id"], "credential": cred })),
            &amy,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["result_status"], 200, "{done}");
    assert_eq!(done["result_body"]["matches"][0]["key"], "K");
    assert!(!done.to_string().contains("CANARY"), "{done}");

    let log = log_text(&h).await;
    assert!(!log.contains("CANARY"), "an event recorded the text: {log}");
    assert_eq!(h.events(KIND).await.len(), 2);
    tdb.cleanup().await;
}

/// The answer and the record carry names and scopes, never the text or a
/// value. A check with no match is recorded too, with a count of 0.
#[tokio::test]
async fn the_answer_and_the_event_carry_names_never_text_or_values() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    environment(&pg, "acme", "production").await;
    put(
        &secrets,
        org_scope("acme"),
        "ORG_TOKEN",
        "VALUE-CANARY-org-0000001",
    )
    .await;
    put(
        &secrets,
        repo_env("acme", "web", "production"),
        "DEPLOY_TOKEN",
        "VALUE-CANARY-web-prod-001",
    )
    .await;
    let h = world(&pg, &secrets);
    let (s, _) = admin(&h, "dan", "acme").await;

    let paste = "PASTE-CANARY-2 VALUE-CANARY-org-0000001 VALUE-CANARY-web-prod-001";
    let (st, body) = check(&h, &s, "acme", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["matches"].as_array().unwrap().len(), 2);
    assert!(!body.to_string().contains("CANARY"), "{body}");

    let events = h.events(KIND).await;
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.subject, "dan");
    assert_eq!(ev.caller, "session:cookie");
    assert_eq!(ev.org.as_deref(), Some("acme"));
    assert_eq!(ev.op.as_deref(), Some("secret.match"));
    assert_eq!(ev.target.as_deref(), Some("acme"));
    assert!(ev.confirmation_id.is_some(), "linked to the step-up");
    assert_eq!(ev.detail["matches"], 2);
    let matched = ev.detail["matched"].as_array().unwrap();
    assert!(matched.contains(&json!({
        "key": "ORG_TOKEN", "scope": "org", "repo": null, "environment": null
    })));
    assert!(matched.contains(&json!({
        "key": "DEPLOY_TOKEN", "scope": "repo_environment", "repo": "web",
        "environment": "production"
    })));

    let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-3 nothing stored here").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["matches"], json!([]));
    let events = h.events(KIND).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].detail["matches"], 0);
    assert_eq!(events[1].detail["matched"], json!([]));

    let log = log_text(&h).await;
    assert!(!log.contains("CANARY"), "{log}");
    tdb.cleanup().await;
}

/// Twenty checks in an hour pass; the twenty-first is refused with when the
/// next is allowed, until an hour after the first. Another Admin, and the
/// same Admin in another org, are counted apart.
#[tokio::test]
async fn the_twenty_first_check_in_an_hour_is_refused_until_the_window_rolls() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    let h = world(&pg, &secrets);
    let (s, mut soft) = admin(&h, "dan", "acme").await;
    h.grant("dan", Scope::Org("globex".into()), Role::Admin)
        .await;
    let (eve, _) = admin(&h, "eve", "acme").await;

    let first = h.now().await;
    for i in 0..20 {
        let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-4").await;
        assert_eq!(st, StatusCode::OK, "check {i}: {body}");
        h.clock.advance(1_000);
    }
    let resp = h
        .app
        .clone()
        .oneshot(cookie_req(
            "POST",
            "/v1/orgs/acme/secrets/match",
            Some(json!({ "text": "PASTE-CANARY-4" })),
            &s,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let now = h.now().await;
    let retry_after: i64 = resp.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], "rate_limited");
    assert_eq!(body["retry_at"], first + HOUR, "{body}");
    assert_eq!(retry_after, (first + HOUR - now + 999) / 1000);
    assert_eq!(h.events(KIND).await.len(), 20, "a refusal is not counted");

    // Counted per Admin and per org.
    let (st, body) = check(&h, &eve, "acme", "PASTE-CANARY-4").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = check(&h, &s, "globex", "PASTE-CANARY-4").await;
    assert_eq!(st, StatusCode::OK, "{body}");

    // One millisecond before the first check is an hour old: still refused.
    h.clock.advance(first + HOUR - 1 - h.now().await);
    h.open_window(&s, &mut soft).await;
    let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-4").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{body}");
    h.clock.advance(1);
    let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-4").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    tdb.cleanup().await;
}
