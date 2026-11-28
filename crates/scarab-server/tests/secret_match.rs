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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use axum::http::StatusCode;
use serde_json::{json, Value};
use tower::ServiceExt;

use common::fresh_pg;
use scarab_db_postgres::PostgresDb;
use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_identity::{
    api_token_hash, ApiToken, ApiTokenStore, IdentityError, LoginKind, OrgSecurityEventLog, Role,
    Scope, SecurityEvent, SecurityEventFilter,
};
use scarab_plugin::{
    ConnectionSlot, InstallationState, PluginCapability, PluginConnection,
    PluginConnectionAuthScheme, PluginInstallation, PluginManifest, PluginStore, RequestRule,
    RuntimeStatus,
};
use scarab_project::{OrgEnvironment, OrgEnvironmentStore};
use scarab_secrets::{Secret, SecretError, SecretProvider, SecretScope};
use scarab_testkit::FakeSecrets;
use scarab_webauthn::soft::SoftPasskey;

use step_up_harness::*;

const KIND: &str = "secret.match_checked";
const HOUR: i64 = 60 * 60 * 1000;
const DAY: i64 = 24 * HOUR;

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
            .with_plugins(pg.clone(), scarab_server::plugins::PluginPolicy::off())
            .with_leak_check_limits(pg)
    })
}

/// [`world`], with the event log the app writes wrapped by `wrap`.
fn world_wrapping_log(
    pg: &Arc<PostgresDb>,
    secrets: &Arc<FakeSecrets>,
    wrap: impl FnOnce(Arc<dyn OrgSecurityEventLog>) -> Arc<dyn OrgSecurityEventLog>,
) -> Harness {
    world_on(pg, secrets.clone(), wrap)
}

/// [`world_wrapping_log`] over any secret store.
fn world_on(
    pg: &Arc<PostgresDb>,
    secrets: Arc<dyn SecretProvider>,
    wrap: impl FnOnce(Arc<dyn OrgSecurityEventLog>) -> Arc<dyn OrgSecurityEventLog>,
) -> Harness {
    let pg = pg.clone();
    harness_on_wrapping_log(Backend::Pg(pg.clone()), wrap, move |st| {
        st.with_secrets(secrets)
            .with_org_environments(pg.clone())
            .with_forge_connections(pg.clone())
            .with_plugins(pg.clone(), scarab_server::plugins::PluginPolicy::off())
            .with_leak_check_limits(pg)
    })
}

/// An event log that refuses to record a check (every other event goes
/// through).
struct RefusesCheckRecords(Arc<dyn OrgSecurityEventLog>);

#[async_trait::async_trait]
impl OrgSecurityEventLog for RefusesCheckRecords {
    async fn append(&self, event: &SecurityEvent) -> Result<i64, IdentityError> {
        if event.kind == KIND {
            return Err(IdentityError::Conflict("the log is down".into()));
        }
        self.0.append(event).await
    }

    async fn list(
        &self,
        filter: &SecurityEventFilter,
    ) -> Result<Vec<SecurityEvent>, IdentityError> {
        self.0.list(filter).await
    }
}

/// An event log that, once armed, counts the counts the limits make (a list
/// of one subject's checks).
#[derive(Default)]
struct CountsTheCounts {
    inner: OnceLock<Arc<dyn OrgSecurityEventLog>>,
    armed: AtomicBool,
    /// The limit's counts made since arming.
    counts: AtomicUsize,
}

#[async_trait::async_trait]
impl OrgSecurityEventLog for CountsTheCounts {
    async fn append(&self, event: &SecurityEvent) -> Result<i64, IdentityError> {
        self.inner.get().unwrap().append(event).await
    }

    async fn list(
        &self,
        filter: &SecurityEventFilter,
    ) -> Result<Vec<SecurityEvent>, IdentityError> {
        let a_count = filter.subject.is_some() && filter.kinds == [KIND.to_string()];
        if a_count && self.armed.load(Ordering::SeqCst) {
            self.counts.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.get().unwrap().list(filter).await
    }
}

/// A secret store that, once armed, holds the first two searches (each
/// reads the repositories' keys once) until both have begun. A search runs
/// after the count taken in turn and before the record, so two concurrent
/// checks then both pass that count with the same number.
struct MeetInTheSearch {
    inner: Arc<FakeSecrets>,
    armed: AtomicBool,
    searches: AtomicUsize,
    both: tokio::sync::Barrier,
}

#[async_trait::async_trait]
impl SecretProvider for MeetInTheSearch {
    async fn get(&self, scope: &SecretScope, key: &str) -> Result<Secret, SecretError> {
        self.inner.get(scope, key).await
    }

    async fn put(&self, scope: &SecretScope, secret: Secret) -> Result<(), SecretError> {
        self.inner.put(scope, secret).await
    }

    async fn list_scoped(&self, scope: &SecretScope) -> Result<Vec<String>, SecretError> {
        self.inner.list_scoped(scope).await
    }

    async fn delete(&self, scope: &SecretScope, key: &str) -> Result<(), SecretError> {
        self.inner.delete(scope, key).await
    }

    async fn environment_secret_projects(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<String>, SecretError> {
        self.inner
            .environment_secret_projects(org, environment)
            .await
    }

    async fn environment_secret_scopes(
        &self,
        org: &str,
    ) -> Result<Vec<(String, String)>, SecretError> {
        self.inner.environment_secret_scopes(org).await
    }

    async fn project_secret_keys(
        &self,
        org: &str,
    ) -> Result<Vec<(String, Option<String>, String)>, SecretError> {
        if self.armed.load(Ordering::SeqCst) && self.searches.fetch_add(1, Ordering::SeqCst) < 2 {
            self.both.wait().await;
        }
        self.inner.project_secret_keys(org).await
    }

    async fn org_environment_secret_environments(
        &self,
        org: &str,
    ) -> Result<Vec<String>, SecretError> {
        self.inner.org_environment_secret_environments(org).await
    }
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
                extensions: None,
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

/// An org Admin of `org` in a browser, with a passkey.
async fn admin(h: &Harness, subject: &str, org: &str) -> (Sess, SoftPasskey) {
    member(h, subject, org, Role::Admin).await
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

/// A check as `s` that confirms with `key` when asked: each check is asked
/// for a confirmation of its own (mode `action`), answered, and sent again.
/// A check refused before the ask (or answered without one) is returned as
/// it is.
async fn checked(
    h: &Harness,
    s: &Sess,
    key: &mut SoftPasskey,
    org: &str,
    text: &str,
) -> (StatusCode, Value) {
    let (st, body) = check(h, s, org, text).await;
    if !is_step_up(st, &body) {
        return (st, body);
    }
    assert_eq!(body["mode"], "action", "{body}");
    let (st, conf) = h.answer(s, key, &body["challenge"]).await;
    assert_eq!(st, StatusCode::OK, "{conf}");
    check(h, s, org, text).await
}

/// `PUT /v1/orgs/{org}/security/leak-checks` as `s`, confirming with `key`
/// when asked.
async fn set_limits(
    h: &Harness,
    s: &Sess,
    key: &mut SoftPasskey,
    org: &str,
    per_hour: i64,
    per_day: i64,
) -> (StatusCode, Value) {
    let uri = format!("/v1/orgs/{org}/security/leak-checks");
    let body = json!({ "per_hour": per_hour, "per_day": per_day });
    let (st, b) = h.cookie("PUT", &uri, Some(body.clone()), s).await;
    if !is_step_up(st, &b) {
        return (st, b);
    }
    assert_eq!(b["mode"], "action", "{b}");
    let (st, conf) = h.answer(s, key, &b["challenge"]).await;
    assert_eq!(st, StatusCode::OK, "{conf}");
    h.cookie("PUT", &uri, Some(body), s).await
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
                kind: scarab_identity::PrincipalTokenKind::Issued,
                operations: None,
                audience: None,
                oauth_client: None,
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
    let (s, mut key) = admin(&h, "dan", "acme").await;

    let paste = "PASTE-CANARY-env\n\
                 ORG=VALUE-CANARY-org-0000001\n\
                 OE=VALUE-CANARY-orgenv-00001\n\
                 WEB=VALUE-CANARY-repo-web-001\n\
                 WP=VALUE-CANARY-web-prod-001\n\
                 F=VALUE-CANARY-forge-000001\n\
                 P=VALUE-CANARY-plugin-00001\n";
    let (st, body) = checked(&h, &s, &mut key, "acme", paste).await;
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
    let (s, mut key) = admin(&h, "dan", "acme").await;
    let paste = "leaked: VALUE-CANARY-shared-0001";

    let (st, body) = checked(&h, &s, &mut key, "acme", paste).await;
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
    let (st, body) = checked(&h, &s, &mut key, "acme", paste).await;
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
    let (s, mut key) = admin(&h, "dan", "acme").await;
    h.grant("dan", Scope::Org("globex".into()), Role::Admin)
        .await;
    let paste = "VALUE-CANARY-shared-0001 VALUE-CANARY-globex-web-1 \
                 VALUE-CANARY-forge-globex VALUE-CANARY-plugin-globx";

    let (st, body) = checked(&h, &s, &mut key, "acme", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["matches"], json!([]), "{body}");
    assert_eq!(
        body["searched"],
        json!({ "secrets": 1, "repositories": 0, "connections": 0 })
    );

    // The same paste in globex finds all four: the fixture is live.
    let (st, body) = checked(&h, &s, &mut key, "globex", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["matches"].as_array().unwrap().len(), 4, "{body}");

    tdb.cleanup().await;
}

/// A GitHub installation assigned to the org points at the server's own
/// App key, shared by every installation of every org: it is not compared
/// and not counted. The org's own Forgejo token still is.
#[tokio::test]
async fn the_shared_github_app_key_is_not_compared() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    ForgeConnectionStore::put_connection(
        &*pg,
        &ForgeConnection {
            id: "github-install-42".into(),
            kind: ForgeKind::GitHub,
            base_url: "https://api.github.com".into(),
            credential_ref: scarab_server::GITHUB_APP_CREDENTIAL_REF.into(),
            org: Some("acme".into()),
            installation_id: Some(42),
            account_id: None,
            account_login: Some("acme".into()),
        },
    )
    .await
    .unwrap();
    put(
        &secrets,
        org_scope(scarab_server::FORGE_CREDENTIALS_ORG),
        scarab_server::GITHUB_APP_CREDENTIAL_REF,
        "VALUE-CANARY-github-app-key",
    )
    .await;
    let forgejo = forge(&pg, &secrets, "acme", "VALUE-CANARY-forge-000001").await;
    let h = world(&pg, &secrets);
    let (s, mut key) = admin(&h, "dan", "acme").await;

    let paste = "VALUE-CANARY-github-app-key VALUE-CANARY-forge-000001";
    let (st, body) = checked(&h, &s, &mut key, "acme", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let matches = body["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{body}");
    assert_eq!(matches[0]["id"], forgejo);
    assert_eq!(body["searched"]["connections"], 1, "{body}");

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
    let (s, mut key) = admin(&h, "dan", "acme").await;

    for blank in ["", "   ", "\n\t \r\n"] {
        let (st, body) = checked(&h, &s, &mut key, "acme", blank).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "empty");
    }
    let (st, body) = checked(&h, &s, &mut key, "acme", &"a".repeat(4097)).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["code"], "too_long");
    assert_eq!(body["max_bytes"], 4096);
    let (st, body) = checked(&h, &s, &mut key, "acme", &"a".repeat(4096)).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // 2049 two-byte characters: 4098 bytes.
    let (st, body) = checked(&h, &s, &mut key, "acme", &"é".repeat(2049)).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    let (st, body) = checked(&h, &s, &mut key, "acme", &"é".repeat(2048)).await;
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
// C. Step-up, browser only, the record
// ---------------------------------------------------------------------------

/// Every check needs a passkey confirmation of its own: a window does not
/// count, and a confirmation is spent by the one check it was made for.
/// Neither the refusal nor the event log shows the text.
#[tokio::test]
async fn every_check_needs_a_confirmation_of_its_own() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    put(&secrets, org_scope("acme"), "K", "VALUE-CANARY-org-0000001").await;
    let h = world(&pg, &secrets);
    let paste = "PASTE-CANARY-1 VALUE-CANARY-org-0000001";
    let (s, mut key) = admin(&h, "dan", "acme").await;

    // An open window does not satisfy a check.
    h.open_window(&s, &mut key).await;
    let (st, body) = check(&h, &s, "acme", paste).await;
    assert_step_up(st, &body, "action");
    assert_eq!(body["op"], "secret.match");
    assert_eq!(body["summary"]["target"], "acme");
    assert_eq!(body["summary"]["changes"], json!([]));
    assert!(!body.to_string().contains("CANARY"), "{body}");

    // Confirmed, the check runs once.
    let (st, conf) = h.answer(&s, &mut key, &body["challenge"]).await;
    assert_eq!(st, StatusCode::OK, "{conf}");
    let (st, body) = check(&h, &s, "acme", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["matches"].as_array().unwrap().len(), 1);

    // The second check is asked again, whatever it pastes.
    let (st, body) = check(&h, &s, "acme", paste).await;
    assert_step_up(st, &body, "action");
    let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-other").await;
    assert_step_up(st, &body, "action");
    let (st, body) = checked(&h, &s, &mut key, "acme", paste).await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let log = log_text(&h).await;
    assert!(!log.contains("CANARY"), "an event recorded the text: {log}");
    assert_eq!(h.events(KIND).await.len(), 2);
    tdb.cleanup().await;
}

/// An API token, and a session presented as Bearer, are refused: a plain
/// 403 `browser_session_required`, no confirmation request raised, nothing
/// parked or checked.
#[tokio::test]
async fn an_api_token_or_a_bearer_session_is_refused() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    put(&secrets, org_scope("acme"), "K", "VALUE-CANARY-org-0000001").await;
    let h = world(&pg, &secrets);
    h.grant("amy", Scope::Org("acme".into()), Role::Admin).await;
    let amy = h.browser_session(scoped("amy")).await;
    let mut amy_key = passkey();
    h.enroll(&amy, &mut amy_key, "Laptop").await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let cli = h.session(scoped("amy"), LoginKind::Api).await;

    for bearer in [token.as_str(), cli.id.as_str()] {
        let (st, body) = h
            .bearer(
                "POST",
                "/v1/orgs/acme/secrets/match",
                Some(json!({ "text": "PASTE-CANARY-1 VALUE-CANARY-org-0000001" })),
                bearer,
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "browser_session_required", "{body}");
        assert!(body.get("request_id").is_none(), "{body}");
        assert!(!body.to_string().contains("CANARY"), "{body}");
    }
    assert!(h.events("request.raised").await.is_empty());
    assert!(h.events(KIND).await.is_empty());
    let log = log_text(&h).await;
    assert!(!log.contains("CANARY"), "{log}");
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
    let (s, mut key) = admin(&h, "dan", "acme").await;

    let paste = "PASTE-CANARY-2 VALUE-CANARY-org-0000001 VALUE-CANARY-web-prod-001";
    let (st, body) = checked(&h, &s, &mut key, "acme", paste).await;
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

    let (st, body) = checked(
        &h,
        &s,
        &mut key,
        "acme",
        "PASTE-CANARY-3 nothing stored here",
    )
    .await;
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

// ---------------------------------------------------------------------------
// D. The org's limits
// ---------------------------------------------------------------------------

/// With the default limits, twenty checks in an hour pass; the
/// twenty-first is refused with when the next is allowed, until an hour
/// after the first. Another Admin, and the same Admin in another org, are
/// counted apart.
#[tokio::test]
async fn the_twenty_first_check_in_an_hour_is_refused_until_the_window_rolls() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    let h = world(&pg, &secrets);
    let (s, mut key) = admin(&h, "dan", "acme").await;
    h.grant("dan", Scope::Org("globex".into()), Role::Admin)
        .await;
    let (eve, mut eve_key) = admin(&h, "eve", "acme").await;

    let first = h.now().await;
    for i in 0..20 {
        let (st, body) = checked(&h, &s, &mut key, "acme", "PASTE-CANARY-4").await;
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
    let (st, body) = checked(&h, &eve, &mut eve_key, "acme", "PASTE-CANARY-4").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = checked(&h, &s, &mut key, "globex", "PASTE-CANARY-4").await;
    assert_eq!(st, StatusCode::OK, "{body}");

    // One millisecond before the first check is an hour old: still refused,
    // before any confirmation is asked for.
    h.clock.advance(first + HOUR - 1 - h.now().await);
    let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-4").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{body}");
    h.clock.advance(1);
    let (st, body) = checked(&h, &s, &mut key, "acme", "PASTE-CANARY-4").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    tdb.cleanup().await;
}

/// With the default limits, forty checks in a day pass, twenty in each of
/// two hours; the forty-first is refused although its hour is empty, until
/// a day after the first. While both are reached, `retry_at` is when both
/// allow a check: the later of the two.
#[tokio::test]
async fn the_forty_first_check_in_a_day_is_refused_until_the_day_rolls() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    let h = world(&pg, &secrets);
    let (s, mut key) = admin(&h, "dan", "acme").await;

    let first = h.now().await;
    // The second twenty start once the first twenty are all over an hour
    // old.
    for start in [first, first + HOUR + 30_000] {
        h.clock.advance(start - h.now().await);
        for i in 0..20 {
            let (st, body) = checked(&h, &s, &mut key, "acme", "PASTE-CANARY-8").await;
            assert_eq!(st, StatusCode::OK, "from {start}, check {i}: {body}");
            h.clock.advance(1_000);
        }
    }
    // Both reached: the hour allows one an hour after the second twenty
    // started, the day a day after the first check.
    let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-8").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["retry_at"], first + DAY, "{body}");

    // The hour is empty again; the day is not.
    h.clock.advance(first + 3 * HOUR - h.now().await);
    let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-8").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "rate_limited");
    assert_eq!(body["retry_at"], first + DAY, "{body}");
    assert_eq!(h.events(KIND).await.len(), 40);

    // A session lasts a day: dan signs in again.
    h.clock.advance(first + DAY - h.now().await);
    let s = h.browser_session(scoped("dan")).await;
    let (st, body) = checked(&h, &s, &mut key, "acme", "PASTE-CANARY-8").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    tdb.cleanup().await;
}

/// The check reads the org's limits: an hourly limit an Owner lowered to 2
/// refuses the third check in the hour; another org keeps the defaults.
#[tokio::test]
async fn a_lowered_hourly_limit_takes_effect() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    let h = world(&pg, &secrets);
    let (olga, mut olga_key) = member(&h, "olga", "acme", Role::Owner).await;
    let (st, body) = set_limits(&h, &olga, &mut olga_key, "acme", 2, 40).await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let (s, mut key) = admin(&h, "dan", "acme").await;
    h.grant("dan", Scope::Org("globex".into()), Role::Admin)
        .await;
    let first = h.now().await;
    for i in 0..2 {
        let (st, body) = checked(&h, &s, &mut key, "acme", "PASTE-CANARY-9").await;
        assert_eq!(st, StatusCode::OK, "check {i}: {body}");
        h.clock.advance(1_000);
    }
    let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-9").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["retry_at"], first + HOUR, "{body}");
    let (st, body) = checked(&h, &s, &mut key, "globex", "PASTE-CANARY-9").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    tdb.cleanup().await;
}

/// A limit of 0 turns the check off for the org: 403 `leak_checks_off`,
/// before any confirmation is asked for, and nothing is checked.
#[tokio::test]
async fn a_limit_of_zero_turns_the_check_off() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    let h = world(&pg, &secrets);
    let (olga, mut olga_key) = member(&h, "olga", "acme", Role::Owner).await;
    let (s, _) = admin(&h, "dan", "acme").await;
    for (per_hour, per_day) in [(0, 40), (20, 0)] {
        let (st, body) = set_limits(&h, &olga, &mut olga_key, "acme", per_hour, per_day).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-10").await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "leak_checks_off", "{body}");
        assert!(body.get("challenge").is_none(), "{body}");
    }
    let refused = h.events("step_up.refused").await;
    assert!(
        refused
            .iter()
            .all(|e| e.op.as_deref() != Some("secret.match")),
        "{refused:?}"
    );
    assert!(h.events(KIND).await.is_empty());
    tdb.cleanup().await;
}

/// The defaults, the bounds and whether the caller may change them, for an
/// org no Owner has changed; an Admin reads them, a Member may not.
#[tokio::test]
async fn an_org_without_limits_has_the_defaults() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    let h = world(&pg, &secrets);
    let uri = "/v1/orgs/acme/security/leak-checks";
    let (s, _) = admin(&h, "dan", "acme").await;
    let (st, body) = h.cookie("GET", uri, None, &s).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({
            "per_hour": 20, "per_day": 40, "max_per_hour": 100, "max_per_day": 1000,
            "caller_role": "admin"
        })
    );
    let (olga, _) = member(&h, "olga", "acme", Role::Owner).await;
    let (st, body) = h.cookie("GET", uri, None, &olga).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["caller_role"], "owner");
    let (mia, _) = member(&h, "mia", "acme", Role::Member).await;
    let (st, body) = h.cookie("GET", uri, None, &mia).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    tdb.cleanup().await;
}

/// Only an org Owner changes the limits, within the bounds, with a
/// confirmation of its own that shows the old and the new values; the
/// change is recorded with both.
#[tokio::test]
async fn an_owner_changes_the_limits_with_a_confirmation_and_a_record() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    let h = world(&pg, &secrets);
    let uri = "/v1/orgs/acme/security/leak-checks";

    // An Admin may not.
    let (dan, mut dan_key) = admin(&h, "dan", "acme").await;
    let (st, body) = set_limits(&h, &dan, &mut dan_key, "acme", 50, 200).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_ne!(body["code"], "step_up_required", "{body}");

    // Out of bounds, or not the body.
    let (olga, mut olga_key) = member(&h, "olga", "acme", Role::Owner).await;
    for (per_hour, per_day) in [(101, 40), (20, 1001), (-1, 40), (20, -1)] {
        let (st, body) = h
            .cookie(
                "PUT",
                uri,
                Some(json!({ "per_hour": per_hour, "per_day": per_day })),
                &olga,
            )
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{per_hour}/{per_day}: {body}");
        assert_eq!(body["code"], "out_of_bounds", "{body}");
    }
    let (st, body) = h
        .cookie("PUT", uri, Some(json!({ "per_hour": 20 })), &olga)
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "bad_body", "{body}");

    // Within bounds: asked, with the old and the new values.
    h.open_window(&olga, &mut olga_key).await;
    let change = json!({ "per_hour": 50, "per_day": 200 });
    let (st, body) = h.cookie("PUT", uri, Some(change.clone()), &olga).await;
    assert_step_up(st, &body, "action");
    assert_eq!(body["op"], "secret.match_limits");
    assert_eq!(body["summary"]["target"], "acme");
    assert_eq!(
        body["summary"]["changes"],
        json!(["per_hour = 20 → 50", "per_day = 40 → 200"])
    );
    let (st, conf) = h.answer(&olga, &mut olga_key, &body["challenge"]).await;
    assert_eq!(st, StatusCode::OK, "{conf}");
    let (st, body) = h.cookie("PUT", uri, Some(change), &olga).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({
            "per_hour": 50, "per_day": 200, "max_per_hour": 100, "max_per_day": 1000,
            "caller_role": "owner"
        })
    );
    let (st, body) = h.cookie("GET", uri, None, &dan).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        (body["per_hour"].clone(), body["per_day"].clone()),
        (json!(50), json!(200))
    );

    let events = h.events("secret.match_limits_changed").await;
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.subject, "olga");
    assert_eq!(ev.org.as_deref(), Some("acme"));
    assert_eq!(ev.op.as_deref(), Some("secret.match_limits"));
    assert!(ev.confirmation_id.is_some(), "linked to the step-up");
    assert_eq!(
        ev.detail,
        json!({
            "old": { "per_hour": 20, "per_day": 40 },
            "new": { "per_hour": 50, "per_day": 200 }
        })
    );

    // The next change asks again; one that changes nothing does not.
    let (st, body) = h
        .cookie(
            "PUT",
            uri,
            Some(json!({ "per_hour": 10, "per_day": 200 })),
            &olga,
        )
        .await;
    assert_step_up(st, &body, "action");
    assert_eq!(body["summary"]["changes"], json!(["per_hour = 50 → 10"]));
    let (st, body) = h
        .cookie(
            "PUT",
            uri,
            Some(json!({ "per_hour": 50, "per_day": 200 })),
            &olga,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(h.events("secret.match_limits_changed").await.len(), 1);
    tdb.cleanup().await;
}

/// A check that cannot be recorded is not answered: a 503 that names
/// nothing it found, and no record.
#[tokio::test]
async fn a_check_that_cannot_be_recorded_is_not_answered() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    put(
        &secrets,
        org_scope("acme"),
        "LEAKED_KEY",
        "VALUE-CANARY-org-0000001",
    )
    .await;
    let h = world_wrapping_log(&pg, &secrets, |log| Arc::new(RefusesCheckRecords(log)));
    let (s, mut key) = admin(&h, "dan", "acme").await;

    let (st, body) = checked(
        &h,
        &s,
        &mut key,
        "acme",
        "PASTE-CANARY-6 VALUE-CANARY-org-0000001",
    )
    .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let text = body.to_string();
    for absent in ["matches", "LEAKED_KEY", "CANARY", "searched"] {
        assert!(!text.contains(absent), "the 503 holds {absent}: {text}");
    }
    assert!(h.events(KIND).await.is_empty());
    tdb.cleanup().await;
}

/// With 19 checks in the hour, two concurrent checks by the same Admin in
/// the same org (from two browsers, each with a confirmation of its own)
/// both pass the counts taken before the step-up and in turn before the
/// search (each sees 19), and meet in the search; the count taken again in
/// turn after it lets exactly one through.
#[tokio::test]
async fn two_concurrent_checks_cannot_both_take_the_last_slot() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let counter = Arc::new(CountsTheCounts::default());
    let search = Arc::new(MeetInTheSearch {
        inner: Arc::new(FakeSecrets::new()),
        armed: AtomicBool::new(false),
        searches: AtomicUsize::new(0),
        both: tokio::sync::Barrier::new(2),
    });
    let c = counter.clone();
    let h = world_on(&pg, search.clone(), move |log| {
        c.inner.set(log).ok().expect("wrapped once");
        c
    });
    let (s, mut key) = admin(&h, "dan", "acme").await;
    for i in 0..19 {
        let (st, body) = checked(&h, &s, &mut key, "acme", "PASTE-CANARY-7").await;
        assert_eq!(st, StatusCode::OK, "check {i}: {body}");
    }
    // A second browser: a session holds one unspent confirmation for a
    // check, so each concurrent check is confirmed in a session of its own.
    let s2 = h.browser_session(scoped("dan")).await;
    for sess in [&s, &s2] {
        let (st, body) = check(&h, sess, "acme", "PASTE-CANARY-7").await;
        assert_step_up(st, &body, "action");
        let (st, conf) = h.answer(sess, &mut key, &body["challenge"]).await;
        assert_eq!(st, StatusCode::OK, "{conf}");
    }

    counter.armed.store(true, Ordering::SeqCst);
    search.armed.store(true, Ordering::SeqCst);
    let send = |sess: &Sess| {
        h.app.clone().oneshot(cookie_req(
            "POST",
            "/v1/orgs/acme/secrets/match",
            Some(json!({ "text": "PASTE-CANARY-7" })),
            sess,
        ))
    };
    let (a, b) = tokio::join!(send(&s), send(&s2));
    let mut statuses = [a.unwrap().status(), b.unwrap().status()];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::TOO_MANY_REQUESTS],
        "exactly one takes the twentieth slot"
    );
    assert_eq!(search.searches.load(Ordering::SeqCst), 2, "both searched");
    // Each check counted three times (the hour and the day each time):
    // before the step-up, in turn before the search and in turn after it.
    // The 429 came from the last.
    assert_eq!(counter.counts.load(Ordering::SeqCst), 12);
    assert_eq!(h.events(KIND).await.len(), 20);
    tdb.cleanup().await;
}

/// A new confirmation of a check replaces the session's unspent one: the
/// confirmation does not commit to the text, so two unspent ones would let
/// two checks run on one. Two checks are asked, both are answered; one
/// check runs, on the second confirmation, and the next is asked again.
#[tokio::test]
async fn a_new_confirmation_replaces_the_unspent_one() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    let h = world(&pg, &secrets);
    let (s, mut key) = admin(&h, "dan", "acme").await;

    let (st, first) = check(&h, &s, "acme", "PASTE-CANARY-11 text A").await;
    assert_step_up(st, &first, "action");
    let (st, second) = check(&h, &s, "acme", "PASTE-CANARY-11 text B").await;
    assert_step_up(st, &second, "action");
    let (st, _) = h.answer(&s, &mut key, &first["challenge"]).await;
    assert_eq!(st, StatusCode::OK);
    let (st, conf) = h.answer(&s, &mut key, &second["challenge"]).await;
    assert_eq!(st, StatusCode::OK, "{conf}");

    let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-11 text A").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-11 text B").await;
    assert_step_up(st, &body, "action");

    let events = h.events(KIND).await;
    assert_eq!(events.len(), 1, "one confirmation, one check");
    assert_eq!(
        events[0].confirmation_id.as_deref(),
        conf["confirmation_id"].as_str(),
        "the check ran on the second confirmation"
    );
    tdb.cleanup().await;
}

/// An API token and a session presented as Bearer may not change the
/// limits, not even with a request that changes nothing.
#[tokio::test]
async fn an_api_token_or_a_bearer_session_may_not_change_the_limits() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    let h = world(&pg, &secrets);
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let cli = h.session(scoped("amy"), LoginKind::Api).await;
    let uri = "/v1/orgs/acme/security/leak-checks";

    for bearer in [token.as_str(), cli.id.as_str()] {
        for (per_hour, per_day) in [(50, 200), (20, 40)] {
            let (st, body) = h
                .bearer(
                    "PUT",
                    uri,
                    Some(json!({ "per_hour": per_hour, "per_day": per_day })),
                    bearer,
                )
                .await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
            assert_eq!(body["code"], "browser_session_required", "{body}");
        }
    }
    let (olga, _) = member(&h, "olga", "acme", Role::Owner).await;
    let (st, body) = h.cookie("GET", uri, None, &olga).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        (body["per_hour"].clone(), body["per_day"].clone()),
        (json!(20), json!(40))
    );
    assert!(h.events("request.raised").await.is_empty());
    assert!(h.events("secret.match_limits_changed").await.is_empty());
    tdb.cleanup().await;
}

/// Checks recorded under the caller's handle (rows written before a
/// Principal was named by id) count against its limits too.
#[tokio::test]
async fn checks_recorded_under_the_handle_count() {
    let Some((tdb, pg)) = fresh_pg().await else {
        return;
    };
    let secrets = Arc::new(FakeSecrets::new());
    let h = world(&pg, &secrets);
    h.grant("p_dan", Scope::Org("acme".into()), Role::Admin)
        .await;
    let s = h
        .browser_session(scarab_identity::Principal {
            subject: "p_dan".into(),
            handle: "dan".into(),
            display_name: None,
            roles: vec![],
        })
        .await;
    let mut key = passkey();
    h.enroll(&s, &mut key, "dan's key").await;

    let now = h.now().await;
    for i in 0..20 {
        h.log
            .append(&SecurityEvent {
                seq: 0,
                id: format!("old-check-{i}"),
                org: Some("acme".into()),
                at: now - 60_000 + i,
                kind: KIND.into(),
                subject: "dan".into(),
                caller: "session:cookie".into(),
                op: Some("secret.match".into()),
                target: Some("acme".into()),
                confirmation_id: None,
                request_id: None,
                passkey_id: None,
                session_hash: None,
                detail: json!({}),
            })
            .await
            .unwrap();
    }
    let (st, body) = check(&h, &s, "acme", "PASTE-CANARY-12").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["retry_at"], now - 60_000 + HOUR, "{body}");
    tdb.cleanup().await;
}
