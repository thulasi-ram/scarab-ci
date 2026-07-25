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
    secrets: Arc<FakeSecrets>,
}

impl Harness {
    /// The credential material actually stored under a `_forge` handle — the
    /// same read `connection_credential()` performs, so a test asserts the write
    /// -through landed where the adapter will look for it.
    async fn stored_credential(&self, credential_ref: &str) -> Option<Vec<u8>> {
        self.secrets
            .get(
                &SecretScope::Org {
                    org: FORGE_CREDENTIALS_ORG.into(),
                },
                credential_ref,
            )
            .await
            .ok()
            .map(|s| s.value)
    }
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
            .with_secrets(secrets.clone())
            .with_forge(Arc::new(forge))
            .with_forge_connections(db.clone()),
    );
    Harness {
        app,
        db,
        rbac,
        secrets,
    }
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

fn authed_json(method: &str, uri: &str, session: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {session}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
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

// ---------------------------------------------------------------------------
// Connection CREATE + DELETE with credential write-through (ADR-0060 part D).
//
// This is the path that makes Forgejo onboardable: GitHub registers itself when
// the App is installed, but a Forgejo instance emits no such event, so before
// this the only route into the registry was a hand-written database row.
// ---------------------------------------------------------------------------

/// The token an admin pastes into the form. Distinctive on purpose — every
/// response body in these tests is grepped for it.
const FORGEJO_TOKEN: &str = "fj-pat-do-not-leak-9f3c";

fn create_forgejo(base_url: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "forgejo",
        "base_url": base_url,
        "credential": FORGEJO_TOKEN,
    })
}

#[tokio::test]
async fn creating_a_connection_writes_the_credential_through_and_never_echoes_it() {
    let h = harness(&[("acme", "web")], FakeForge::new(), true).await;
    let root = login(&h.app, "root-code").await;

    // A trailing slash on the base URL is normalized away — the adapter appends
    // `/api/v1`, so `https://git.acme.test//api/v1` would be a 404 per call.
    let resp = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/v1/connections",
            &root,
            create_forgejo("https://git.acme.test/"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created_raw = body_bytes(resp).await;
    let created: serde_json::Value = serde_json::from_str(&created_raw).unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let credential_ref = created["credential_ref"].as_str().unwrap().to_string();
    assert!(id.starts_with("forgejo-"), "id names its forge: {id}");

    // The row persisted, pointing at the generated handle — not at a value.
    let conn = h
        .db
        .get_connection(&id)
        .await
        .unwrap()
        .expect("connection persisted");
    assert_eq!(conn.kind, ForgeKind::Forgejo);
    assert_eq!(conn.base_url, "https://git.acme.test");
    assert_eq!(conn.credential_ref, credential_ref);

    // Write-through: the material is resolvable through the SAME `_forge`-scoped
    // path the adapter uses at call time. Without this the row would exist and
    // every forge call would fail authentication.
    assert_eq!(
        h.stored_credential(&credential_ref).await.as_deref(),
        Some(FORGEJO_TOKEN.as_bytes()),
        "the token is stored under the generated `_forge` handle"
    );

    // The new connection shows up in the list, credential resolving, no repos yet.
    let list_raw = body_bytes(
        h.app
            .clone()
            .oneshot(authed("GET", "/v1/connections", &root))
            .await
            .unwrap(),
    )
    .await;
    let conns: serde_json::Value = serde_json::from_str(&list_raw).unwrap();
    let fj = conns
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == serde_json::json!(id))
        .expect("the created connection is listed");
    assert_eq!(fj["kind"], "forgejo");
    assert_eq!(fj["credential_present"], true);
    assert_eq!(fj["projects"], serde_json::json!([]));
    // Forgejo's base URL IS its web host (its API lives under /api/v1).
    assert_eq!(fj["web_url"], "https://git.acme.test");

    // Write-only, for real: neither the create response nor the read surface
    // contains the token. A credential you can read back is a credential leaked.
    for (what, raw) in [("create", &created_raw), ("list", &list_raw)] {
        assert!(
            !raw.contains(FORGEJO_TOKEN),
            "the credential leaked into the {what} response: {raw}"
        );
    }
}

#[tokio::test]
async fn github_connections_are_not_creatable_by_hand() {
    // Installing the App IS registration (ADR-0060 part C). Accepting a create
    // here would persist a connection to an installation that may not exist —
    // and Scarab has no API to create one.
    let h = harness(&[], FakeForge::new(), true).await;
    let root = login(&h.app, "root-code").await;

    let resp = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/v1/connections",
            &root,
            serde_json::json!({
                "kind": "github",
                "base_url": "https://api.github.com",
                "credential": "ghp_nope",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let msg = body_bytes(resp).await;
    assert!(msg.contains("App"), "the error points at the App: {msg}");
    // Nothing new was written — only the harness's own seeded installation.
    assert_eq!(h.db.list_connections().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_malformed_create_is_refused_before_anything_is_stored() {
    let h = harness(&[], FakeForge::new(), true).await;
    let root = login(&h.app, "root-code").await;

    let cases = [
        // Not a URL: the adapter would build nonsense request URLs per call.
        serde_json::json!({ "kind": "forgejo", "base_url": "git.acme.test", "credential": FORGEJO_TOKEN }),
        // A blank credential is the "I'll fill it in later" trap: the row looks
        // healthy and every call 401s.
        serde_json::json!({ "kind": "forgejo", "base_url": "https://git.acme.test", "credential": "   " }),
        serde_json::json!({ "kind": "gitlab", "base_url": "https://gitlab.com", "credential": "x" }),
    ];
    for body in cases {
        let resp = h
            .app
            .clone()
            .oneshot(authed_json("POST", "/v1/connections", &root, body.clone()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    assert_eq!(h.db.list_connections().await.unwrap().len(), 1, "nothing stored");
}

#[tokio::test]
async fn a_second_connection_to_the_same_host_is_refused() {
    // Two rows for one host would each carry their own credential and each claim
    // to serve it — the shape a double-submitted form produces.
    let h = harness(&[], FakeForge::new(), true).await;
    let root = login(&h.app, "root-code").await;

    let first = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/v1/connections",
            &root,
            create_forgejo("https://git.acme.test"),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let again = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/v1/connections",
            &root,
            create_forgejo("https://git.acme.test"),
        ))
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::CONFLICT);
    // The seeded GitHub installation plus exactly one Forgejo connection.
    assert_eq!(h.db.list_connections().await.unwrap().len(), 2);
}

#[tokio::test]
async fn deleting_a_connection_removes_it_and_its_unreferenced_credential() {
    let h = harness(&[], FakeForge::new(), true).await;
    let root = login(&h.app, "root-code").await;

    let created: serde_json::Value = serde_json::from_str(
        &body_bytes(
            h.app
                .clone()
                .oneshot(authed_json(
                    "POST",
                    "/v1/connections",
                    &root,
                    create_forgejo("https://git.acme.test"),
                ))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let credential_ref = created["credential_ref"].as_str().unwrap().to_string();

    let resp = h
        .app
        .clone()
        .oneshot(authed("DELETE", &format!("/v1/connections/{id}"), &root))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(h.db.get_connection(&id).await.unwrap().is_none());
    // The write-through secret goes with the row it existed for — otherwise a
    // deleted connection leaves live forge credentials behind forever.
    assert!(
        h.stored_credential(&credential_ref).await.is_none(),
        "the orphaned credential was cleaned up"
    );

    // Deleting an unknown connection is a 404, not a silent success.
    let resp = h
        .app
        .clone()
        .oneshot(authed("DELETE", "/v1/connections/nope", &root))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_a_connection_with_projects_needs_an_acknowledgement() {
    // A Project IS a repo binding (ADR-0046), so this delete takes Environments,
    // secrets and RBAC with it. The same reasoning that stops re-sync from ever
    // unbinding applies here — except a human asked, so the answer is "confirm",
    // not "never".
    let h = harness(&[("acme", "web")], FakeForge::new(), true).await;
    let root = login(&h.app, "root-code").await;

    // A SECOND GitHub connection sharing the one App credential handle — exactly
    // how two installations look. Deleting one must not disarm the other.
    h.db.put_connection(&ForgeConnection {
        id: "gh2".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: CRED_REF.into(),
    })
    .await
    .unwrap();

    let resp = h
        .app
        .clone()
        .oneshot(authed("DELETE", "/v1/connections/gh", &root))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let msg = body_bytes(resp).await;
    assert!(msg.contains("acme/web"), "the refusal names what it would delete: {msg}");
    assert!(h.db.get_connection("gh").await.unwrap().is_some());

    // With the acknowledgement, the connection AND its Project go.
    let resp = h
        .app
        .clone()
        .oneshot(authed(
            "DELETE",
            "/v1/connections/gh?unbind_repos=true",
            &root,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let repos = body_bytes(
        h.app
            .clone()
            .oneshot(authed("GET", "/v1/repos", &root))
            .await
            .unwrap(),
    )
    .await;
    assert!(!repos.contains("\"project\":\"web\""), "{repos}");

    // …but the credential the other installation still points at survives.
    assert!(
        h.stored_credential(CRED_REF).await.is_some(),
        "a shared credential is not pulled out from under the remaining connection"
    );
}

#[tokio::test]
async fn create_and_delete_need_administer_on_the_org() {
    let h = harness(&[], FakeForge::new(), true).await;
    let viewer = login(&h.app, "viewer-code").await;

    let resp = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/v1/connections",
            &viewer,
            create_forgejo("https://git.acme.test"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = h
        .app
        .clone()
        .oneshot(authed("DELETE", "/v1/connections/gh", &viewer))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // No connection created, and the seeded one is still there.
    assert_eq!(h.db.list_connections().await.unwrap().len(), 1);
    assert!(h.db.get_connection("gh").await.unwrap().is_some());
}
