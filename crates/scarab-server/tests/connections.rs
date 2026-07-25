//! Feature acceptance for the connections surface (ADR-0060 part C): the
//! read API global Settings renders, and the GitHub **re-sync** that heals a
//! registry which drifted from the forge.
//!
//! Two invariants carry the weight:
//!
//!  1. **Credential material never leaves.** The DTO says whether the handle
//!     resolves; the bytes stay put. A health readout that leaked the secret it
//!     was reporting on would be worse than no readout.
//!  2. **Re-sync only binds.** A Project is a repo binding, and Environments,
//!     secrets and RBAC hang off it — so letting a forge API page decide to
//!     *remove* one would make a transient error destroy governance.
//!
//! Hermetic: InMemoryDb doubles as the connection registry, FakeForge as the
//! forge, FakeSecrets as the credential provider.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_identity::{Binding, BindingOrigin, Principal, RbacStore, Role, Scope};
use scarab_secrets::{Secret, SecretProvider, SecretScope};
use scarab_server::{router, AppState, LogService, FORGE_CREDENTIALS_ORG};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeForge, FakeSecrets, InMemoryDb, InMemoryObjectStore,
    InMemoryRbac, InMemorySessions,
};

const CRED_REF: &str = "github-app";

struct Harness {
    app: axum::Router,
    db: Arc<InMemoryDb>,
    rbac: Arc<InMemoryRbac>,
}

/// A stack with one GitHub connection, auth+RBAC on, and a forge that either can
/// or cannot enumerate repos.
async fn harness(bound: &[(&str, &str)], forge: FakeForge, seed_credential: bool) -> Harness {
    let db = Arc::new(InMemoryDb::new());
    db.put_connection(&ForgeConnection {
        id: "gh".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: CRED_REF.into(),
    })
    .await
    .unwrap();
    for (owner, name) in bound {
        db.bind_repo(
            "gh",
            &RepoRef {
                owner: (*owner).into(),
                name: (*name).into(),
            },
            owner,
            name,
        )
        .await
        .unwrap();
    }

    let secrets = Arc::new(FakeSecrets::new());
    if seed_credential {
        secrets
            .put(
                &SecretScope::Org {
                    org: FORGE_CREDENTIALS_ORG.into(),
                },
                Secret {
                    key: CRED_REF.into(),
                    value: b"-----BEGIN PRIVATE KEY-----super-secret-pem".to_vec(),
                },
            )
            .await
            .unwrap();
    }

    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential(
                "root-code",
                Principal {
                    subject: "root".into(),
                    display_name: None,
                    roles: vec![Role::Owner],
                },
            )
            .with_credential(
                "orgadmin-code",
                Principal {
                    subject: "orgadmin".into(),
                    display_name: None,
                    roles: vec![],
                },
            )
            .with_credential(
                "viewer-code",
                Principal {
                    subject: "viewer".into(),
                    display_name: None,
                    roles: vec![Role::Viewer],
                },
            ),
    );
    let rbac = Arc::new(InMemoryRbac::new());
    let app = router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(0)), logs)
            .with_auth(auth, Arc::new(InMemorySessions::new()))
            .with_rbac(rbac.clone())
            .with_secrets(secrets)
            .with_forge(Arc::new(forge))
            .with_forge_connections(db.clone()),
    );
    Harness { app, db, rbac }
}

async fn body_bytes(resp: axum::response::Response) -> String {
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
    serde_json::from_str::<serde_json::Value>(&body_bytes(resp).await).unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string()
}

fn authed(method: &str, uri: &str, session: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {session}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn the_list_reports_bound_projects_and_health_without_the_credential() {
    let h = harness(
        &[("acme", "web"), ("acme", "api")],
        FakeForge::new().with_accessible_repos(&[("acme", "web")]),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;

    let raw = body_bytes(
        h.app
            .clone()
            .oneshot(authed("GET", "/v1/connections", &root))
            .await
            .unwrap(),
    )
    .await;
    let conns: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let c = &conns[0];

    assert_eq!(c["id"], "gh");
    assert_eq!(c["kind"], "github");
    assert_eq!(c["base_url"], "https://api.github.com");
    // The web host, derived from the API base, is what "Manage on GitHub →"
    // hangs off — api.github.com is not a page a human can open.
    assert_eq!(c["web_url"], "https://github.com");
    assert_eq!(c["credential_ref"], CRED_REF);
    assert_eq!(c["credential_present"], true);
    assert_eq!(c["supports_resync"], true, "GitHub can enumerate");
    assert_eq!(c["managed_by_config"], false);
    // Every bound repo, as its governed Project — a Project IS the binding.
    let projects: Vec<&str> = c["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["project"].as_str().unwrap())
        .collect();
    assert_eq!(projects, vec!["api", "web"], "sorted, both bindings");
    // No delivery has been recorded, so liveness is UNKNOWN — not "broken".
    assert!(c["last_delivery_at"].is_null());

    // The whole payload must not contain the credential material.
    assert!(
        !raw.contains("super-secret-pem") && !raw.contains("BEGIN PRIVATE KEY"),
        "credential material leaked into the connections payload: {raw}"
    );
}

#[tokio::test]
async fn a_missing_credential_is_reported_not_hidden() {
    // The DB was restored without its secrets: the connection row survives, the
    // material does not. Runs will fail at the first forge call; this is the one
    // place that says so before then.
    let h = harness(&[("acme", "web")], FakeForge::new(), false).await;
    let root = login(&h.app, "root-code").await;

    let conns: serde_json::Value = serde_json::from_str(
        &body_bytes(
            h.app
                .clone()
                .oneshot(authed("GET", "/v1/connections", &root))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert_eq!(conns[0]["credential_present"], false);
}

#[tokio::test]
async fn listing_needs_administer_on_the_org() {
    let h = harness(&[("acme", "web")], FakeForge::new(), true).await;

    // An Org-scoped Admin (no global role) may look.
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
    let orgadmin = login(&h.app, "orgadmin-code").await;
    let resp = h
        .app
        .clone()
        .oneshot(authed("GET", "/v1/connections", &orgadmin))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // A Viewer may not — connections span every Project, so this is org-level.
    let viewer = login(&h.app, "viewer-code").await;
    for uri in ["/v1/connections", "/v1/connections/gh/resync"] {
        let method = if uri.ends_with("resync") { "POST" } else { "GET" };
        let resp = h
            .app
            .clone()
            .oneshot(authed(method, uri, &viewer))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
    }
}

#[tokio::test]
async fn resync_binds_a_repo_whose_webhook_was_missed() {
    // GitHub covers three repos; Scarab only knows one — the state after a
    // dropped `installation_repositories` delivery.
    let h = harness(
        &[("acme", "web")],
        FakeForge::new().with_accessible_repos(&[("acme", "web"), ("acme", "api"), ("acme", "ops")]),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;

    let result: serde_json::Value = serde_json::from_str(
        &body_bytes(
            h.app
                .clone()
                .oneshot(authed("POST", "/v1/connections/gh/resync", &root))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    let mut bound: Vec<&str> = result["bound"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_str().unwrap())
        .collect();
    bound.sort();
    assert_eq!(bound, vec!["acme/api", "acme/ops"]);
    assert_eq!(result["confirmed"], 1, "the already-bound repo is confirmed");

    // The healed repos are real Projects now — they resolve, and they appear on
    // the Repos page, which is the whole point.
    for name in ["api", "ops"] {
        let resolved = h
            .db
            .resolve(&RepoRef {
                owner: "acme".into(),
                name: name.into(),
            })
            .await
            .unwrap()
            .expect("bound repo resolves to a Project");
        assert_eq!((resolved.org.as_str(), resolved.project.as_str()), ("acme", name));
    }
    let repos = body_bytes(
        h.app
            .clone()
            .oneshot(authed("GET", "/v1/repos", &root))
            .await
            .unwrap(),
    )
    .await;
    assert!(repos.contains("\"project\":\"ops\""), "{repos}");

    // Idempotent: a second re-sync confirms everything and binds nothing.
    let again: serde_json::Value = serde_json::from_str(
        &body_bytes(
            h.app
                .clone()
                .oneshot(authed("POST", "/v1/connections/gh/resync", &root))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert_eq!(again["bound"], serde_json::json!([]));
    assert_eq!(again["confirmed"], 3);
}

#[tokio::test]
async fn resync_never_unbinds_what_the_forge_omits() {
    // `legacy` is bound in Scarab but the forge does not report it. Unbinding
    // would delete a Project — and its Environments, secrets and RBAC — on the
    // strength of one API response. Removal stays a human act.
    let h = harness(
        &[("acme", "web"), ("acme", "legacy")],
        FakeForge::new().with_accessible_repos(&[("acme", "web")]),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;

    h.app
        .clone()
        .oneshot(authed("POST", "/v1/connections/gh/resync", &root))
        .await
        .unwrap();

    assert!(
        h.db.resolve(&RepoRef {
            owner: "acme".into(),
            name: "legacy".into()
        })
        .await
        .unwrap()
        .is_some(),
        "a repo the forge no longer reports keeps its Project"
    );
}

#[tokio::test]
async fn resync_on_an_adapter_that_cannot_enumerate_says_so() {
    // The default FakeForge cannot enumerate — the Forgejo situation today.
    // A 501 is the honest answer; a silent empty list would read as "your forge
    // has no repos" and, if re-sync ever unbound, as "delete everything".
    let h = harness(&[("acme", "web")], FakeForge::new(), true).await;
    let root = login(&h.app, "root-code").await;

    let resp = h
        .app
        .clone()
        .oneshot(authed("POST", "/v1/connections/gh/resync", &root))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);

    // And an unknown connection is a 404, not a 501.
    let resp = h
        .app
        .clone()
        .oneshot(authed("POST", "/v1/connections/nope/resync", &root))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
