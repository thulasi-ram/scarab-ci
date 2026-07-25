//! Feature acceptance for the global Settings surface (ADR-0060 part A/B):
//! `GET /v1/me` tells the UI whether to render the Settings nav entry at all,
//! and which org it administers, and org-scoped secret CRUD actually works for
//! the principals it says it does.
//!
//! The load-bearing asymmetry: an Org-scoped `Admin` binding grants the org's
//! settings; a Project-scoped one does **not** (ADR-0049 — Org inherits down,
//! never up). Hermetic: InMemoryDb doubles as the connection registry,
//! FakeSecrets as the provider.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_identity::{Binding, BindingOrigin, Principal, RbacStore, Role, Scope};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeSecrets, InMemoryDb, InMemoryObjectStore, InMemoryRbac,
    InMemorySessions,
};

/// A principal with NO global roles — only scoped bindings decide.
fn scoped(subject: &str) -> Principal {
    Principal {
        subject: subject.into(),
        display_name: None,
        roles: vec![],
    }
}

struct Harness {
    app: axum::Router,
    rbac: Arc<InMemoryRbac>,
}

/// A stack with auth on, RBAC on, secrets wired, and the registry available.
/// `bind` names the (org, repo) Projects to register — the only thing that makes
/// an org exist (there is no `orgs` table; ADR-0046).
async fn harness(bind: &[(&str, &str)]) -> Harness {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    if !bind.is_empty() {
        db.put_connection(&ForgeConnection {
            id: "gh".into(),
            kind: ForgeKind::GitHub,
            base_url: "https://api.github.com".into(),
            credential_ref: "gh-token".into(),
        })
        .await
        .unwrap();
        for (org, repo) in bind {
            db.bind_repo(
                "gh",
                &RepoRef {
                    owner: (*org).into(),
                    name: (*repo).into(),
                },
                org,
                repo,
            )
            .await
            .unwrap();
        }
    }
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential(
                "root-code",
                Principal {
                    subject: "root".into(),
                    display_name: None,
                    roles: vec![Role::Owner], // global bootstrap owner
                },
            )
            .with_credential("orgadmin-code", scoped("orgadmin"))
            .with_credential("repoadmin-code", scoped("repoadmin"))
            .with_credential("viewer-code", scoped("viewer")),
    );
    let rbac = Arc::new(InMemoryRbac::new());
    let app = router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(0)), logs)
            .with_auth(auth, Arc::new(InMemorySessions::new()))
            .with_rbac(rbac.clone())
            .with_secrets(Arc::new(FakeSecrets::new()))
            .with_forge_connections(db),
    );
    Harness { app, rbac }
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
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
    body_json(resp).await["session"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn me(app: &axum::Router, session: &str) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/me")
                .header("authorization", format!("Bearer {session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

fn org_secret_req(method: &str, uri: &str, session: &str, body: Option<&str>) -> Request<Body> {
    let b = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {session}"));
    b.body(body.map_or(Body::empty(), |s| Body::from(s.to_string())))
        .unwrap()
}

#[tokio::test]
async fn me_reports_the_org_the_caller_may_administer() {
    let h = harness(&[("acme", "web"), ("acme", "api")]).await;
    let root = login(&h.app, "root-code").await;

    let me = me(&h.app, &root).await;
    assert_eq!(me["can_administer"], true, "global Owner administers");
    assert_eq!(
        me["admin_orgs"],
        serde_json::json!(["acme"]),
        "the one org, deduplicated across its two Projects"
    );
}

#[tokio::test]
async fn a_fresh_install_can_administer_but_has_no_org_yet() {
    // Nothing bound: an org has no existence of its own, so there is nothing to
    // administer *yet* — but the Settings entry must still be reachable, since
    // that is where a connection gets added (ADR-0060 part C).
    let h = harness(&[]).await;
    let root = login(&h.app, "root-code").await;

    let me = me(&h.app, &root).await;
    assert_eq!(me["can_administer"], true);
    assert_eq!(me["admin_orgs"], serde_json::json!([]));
}

#[tokio::test]
async fn an_org_scoped_admin_administers_the_org_and_its_secrets() {
    let h = harness(&[("acme", "web")]).await;
    h.rbac
        .grant(
            &Binding {
                subject: "orgadmin".into(),
                scope: Scope::Org("acme".into()),
                role: Role::Admin,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    let session = login(&h.app, "orgadmin-code").await;

    let me = me(&h.app, &session).await;
    assert_eq!(me["can_administer"], true);
    assert_eq!(me["admin_orgs"], serde_json::json!(["acme"]));

    // …and the org scope it was promised is writable, listable and deletable.
    let resp = h
        .app
        .clone()
        .oneshot(org_secret_req(
            "POST",
            "/v1/secrets",
            &session,
            Some(r#"{"org":"acme","name":"REGISTRY_TOKEN","value":"s3cr3t"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let listed = body_json(
        h.app
            .clone()
            .oneshot(org_secret_req(
                "GET",
                "/v1/secrets?org=acme",
                &session,
                None,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(listed["names"], serde_json::json!(["REGISTRY_TOKEN"]));
    assert!(
        !listed.to_string().contains("s3cr3t"),
        "the value is never returned: {listed}"
    );

    let resp = h
        .app
        .clone()
        .oneshot(org_secret_req(
            "DELETE",
            "/v1/secrets?org=acme&name=REGISTRY_TOKEN",
            &session,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn administering_one_repo_does_not_grant_the_orgs_settings() {
    let h = harness(&[("acme", "web")]).await;
    h.rbac
        .grant(
            &Binding {
                subject: "repoadmin".into(),
                scope: Scope::Project {
                    org: "acme".into(),
                    name: "web".into(),
                },
                role: Role::Admin,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    let session = login(&h.app, "repoadmin-code").await;

    let me = me(&h.app, &session).await;
    assert_eq!(
        me["can_administer"], false,
        "Project Administer must not imply the org's settings (ADR-0049)"
    );
    assert_eq!(me["admin_orgs"], serde_json::json!([]));

    // The claim holds at the API too — the nav is hidden AND the door is shut.
    let resp = h
        .app
        .clone()
        .oneshot(org_secret_req(
            "POST",
            "/v1/secrets",
            &session,
            Some(r#"{"org":"acme","name":"REGISTRY_TOKEN","value":"s3cr3t"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_viewer_gets_no_settings_entry() {
    let h = harness(&[("acme", "web")]).await;
    h.rbac
        .grant(
            &Binding {
                subject: "viewer".into(),
                scope: Scope::Org("acme".into()),
                role: Role::Viewer,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    let session = login(&h.app, "viewer-code").await;

    let me = me(&h.app, &session).await;
    assert_eq!(me["can_administer"], false);
    assert_eq!(me["admin_orgs"], serde_json::json!([]));
    // Keep the harness honest: the binding really is in place, so the `false`
    // above is about the role, not a missing grant.
    assert_eq!(
        h.rbac
            .role_of("viewer", &Scope::Org("acme".into()))
            .await
            .unwrap(),
        Some(Role::Viewer)
    );
}
