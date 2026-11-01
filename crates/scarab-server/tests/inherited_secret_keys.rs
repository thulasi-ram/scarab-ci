//! Feature acceptance for inherited secret keys on a repo's Secrets screen.
//!
//! The screen lists the org's keys beside the repo's own so a repo admin can see
//! every key a Run may resolve. It reads them from the secret matrix, which the
//! repo-level `Administer` permission already opens. What has to hold:
//!
//! - a repo-level admin sees the org's key NAMES, and never a value;
//! - a repo key that the org also defines is marked as overriding the org;
//! - an admin of another org sees none of it.
//!
//! Hermetic: InMemoryDb, FakeSecrets, in-memory RBAC and governance.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_identity::{Binding, BindingOrigin, Principal, RbacStore, Role, Scope};
use scarab_secrets::SecretScope;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeSecrets, InMemoryDb, InMemoryEnvironmentGovernance,
    InMemoryObjectStore, InMemoryRbac, InMemorySessions,
};

/// Org values whose bytes must never appear in a response.
const ORG_TOKEN_VALUE: &str = "org-registry-s3cr3t";
const ORG_URL_VALUE: &str = "org-url-s3cr3t";
const REPO_URL_VALUE: &str = "repo-url-s3cr3t";

fn scoped(subject: &str) -> Principal {
    Principal {
        subject: subject.into(),
        display_name: None,
        roles: vec![],
    }
}

/// `acme/web` and `globex/api` exist; `webadmin` administers acme/web only and
/// `globexadmin` administers globex/api only. acme holds `REGISTRY_TOKEN` and
/// `API_URL` at org scope, and acme/web overrides `API_URL` at repo scope.
async fn app() -> axum::Router {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    // One connection per org: a connection binds only its own org's repos.
    for (org, repo) in [("acme", "web"), ("globex", "api")] {
        let conn = format!("gh-{org}");
        db.put_connection(&ForgeConnection {
            id: conn.clone(),
            kind: ForgeKind::GitHub,
            base_url: "https://api.github.com".into(),
            credential_ref: "gh-token".into(),
            org: Some(org.into()),
            installation_id: None,
            account_id: None,
        })
        .await
        .unwrap();
        db.bind_repo(
            &conn,
            &RepoRef {
                owner: org.into(),
                name: repo.into(),
            },
            org,
            repo,
        )
        .await
        .unwrap();
    }

    let acme = SecretScope::Org { org: "acme".into() };
    let web = SecretScope::Repo {
        org: "acme".into(),
        repo: "web".into(),
    };
    let secrets = FakeSecrets::new()
        .with_secret(&acme, "REGISTRY_TOKEN", ORG_TOKEN_VALUE.as_bytes())
        .with_secret(&acme, "API_URL", ORG_URL_VALUE.as_bytes())
        .with_secret(&web, "API_URL", REPO_URL_VALUE.as_bytes());

    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential("webadmin-code", scoped("webadmin"))
            .with_credential("globexadmin-code", scoped("globexadmin")),
    );
    let rbac = Arc::new(InMemoryRbac::new());
    for (subject, org, name) in [("webadmin", "acme", "web"), ("globexadmin", "globex", "api")] {
        rbac.grant(
            &Binding {
                subject: subject.into(),
                scope: Scope::Project {
                    org: org.into(),
                    name: name.into(),
                },
                role: Role::Admin,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    }

    router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(0)), logs)
            .with_auth(auth, Arc::new(InMemorySessions::new()))
            .with_rbac(rbac)
            .with_secrets(Arc::new(secrets))
            .with_environment_governance(Arc::new(InMemoryEnvironmentGovernance::new()))
            .with_forge_connections(db),
    )
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn login(app: &axum::Router, credential: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "credential": credential }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
    v["session"].as_str().unwrap().to_string()
}

async fn matrix(app: &axum::Router, session: &str, org: &str, repo: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/repos/{org}/{repo}/secrets/matrix"))
                .header("authorization", format!("Bearer {session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, body_text(resp).await)
}

fn row<'a>(m: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    m["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == key)
        .unwrap_or_else(|| panic!("row {key} present in {m}"))
}

#[tokio::test]
async fn a_repo_admin_sees_the_orgs_key_names_but_no_values() {
    let app = app().await;
    let session = login(&app, "webadmin-code").await;

    let (status, body) = matrix(&app, &session, "acme", "web").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let m: serde_json::Value = serde_json::from_str(&body).unwrap();

    // The org key reaches the repo default column, and says it came from the org.
    let token = row(&m, "REGISTRY_TOKEN");
    assert_eq!(token["status"], serde_json::json!({ "": "inherited" }));
    assert_eq!(token["inherited_from"], serde_json::json!({ "": "org" }));

    for value in [ORG_TOKEN_VALUE, ORG_URL_VALUE, REPO_URL_VALUE] {
        assert!(!body.contains(value), "a value leaked into the matrix: {body}");
    }
}

#[tokio::test]
async fn a_repo_key_the_org_also_defines_is_marked_as_overriding_it() {
    let app = app().await;
    let session = login(&app, "webadmin-code").await;

    let (status, body) = matrix(&app, &session, "acme", "web").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let m: serde_json::Value = serde_json::from_str(&body).unwrap();

    let url = row(&m, "API_URL");
    assert_eq!(url["status"], serde_json::json!({ "": "set" }));
    assert_eq!(url["overrides"], serde_json::json!({ "": "org" }));
    // A key only the org holds hides nothing.
    assert_eq!(row(&m, "REGISTRY_TOKEN")["overrides"], serde_json::json!({}));
}

#[tokio::test]
async fn an_admin_of_another_org_sees_none_of_its_keys() {
    let app = app().await;
    let session = login(&app, "globexadmin-code").await;

    // Asking for acme's repo directly is refused, and names nothing.
    let (status, body) = matrix(&app, &session, "acme", "web").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.contains("REGISTRY_TOKEN"), "{body}");

    // Their own repo's matrix inherits from their own org, which holds nothing.
    let (status, body) = matrix(&app, &session, "globex", "api").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("REGISTRY_TOKEN"), "acme's key reached globex: {body}");
    assert!(!body.contains("API_URL"), "acme's key reached globex: {body}");
}
