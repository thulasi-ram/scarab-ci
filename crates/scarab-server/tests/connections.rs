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

use scarab_engine::Db;
use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_identity::{Binding, BindingOrigin, Principal, RbacStore, Role, Scope};
use scarab_secrets::{Secret, SecretProvider, SecretScope};
use scarab_server::{router, AppState, LogService, FORGE_CREDENTIALS_ORG};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeForge, FakeSecrets, InMemoryDb, InMemoryObjectStore,
    InMemoryRbac, InMemorySessions,
};

const CRED_REF: &str = "github-app";
/// The secret `/webhooks/forgejo` verifies deliveries with (ADR-0046).
const FORGEJO_WEBHOOK_SECRET: &[u8] = b"forgejo-hook-secret";

struct Harness {
    app: axum::Router,
    db: Arc<InMemoryDb>,
    rbac: Arc<InMemoryRbac>,
    secrets: Arc<FakeSecrets>,
    forge: Arc<FakeForge>,
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
async fn harness(bound: &[(&str, &str)], forge: Arc<FakeForge>, seed_credential: bool) -> Harness {
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
            .with_forge(forge.clone())
            // The Forgejo ingest endpoint's own verification secret (ADR-0046) —
            // wired so an onboarding test can drive a real signed delivery.
            .with_forgejo_webhook_secret(FORGEJO_WEBHOOK_SECRET.to_vec())
            .with_forge_connections(db.clone()),
    );
    Harness {
        app,
        db,
        rbac,
        secrets,
        forge,
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
        Arc::new(FakeForge::new().with_accessible_repos(&[("acme", "web")])),
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
    let h = harness(&[("acme", "web")], Arc::new(FakeForge::new()), false).await;
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
    let h = harness(&[("acme", "web")], Arc::new(FakeForge::new()), true).await;

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
        Arc::new(FakeForge::new().with_accessible_repos(&[
            ("acme", "web"),
            ("acme", "api"),
            ("acme", "ops"),
        ])),
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
        Arc::new(FakeForge::new().with_accessible_repos(&[("acme", "web")])),
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
    let h = harness(&[("acme", "web")], Arc::new(FakeForge::new()), true).await;
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
// App preflight (git-bug 90644c6): diff the App's granted permissions and
// subscribed events against what Scarab needs.
//
// Both misconfigurations this covers fail SILENTLY today — an App with no event
// subscription still delivers `installation`/`installation_repositories`, so the
// connection registers itself and /v1/repos looks healthy while no run ever
// triggers; an App without `statuses:write` 403s every status post while the run
// goes green. Neither produces an error anyone sees, so the tests assert the gap
// is NAMED, not merely that some health bit flipped.
// ---------------------------------------------------------------------------

/// A GitHub App configured the way the docs say.
const HEALTHY_PERMISSIONS: &[(&str, &str)] = &[
    ("metadata", "read"),
    ("contents", "read"),
    ("statuses", "write"),
    ("pull_requests", "write"),
    ("deployments", "write"),
];
const HEALTHY_EVENTS: &[&str] = &["push", "pull_request", "release", "issue_comment"];

async fn preflight(h: &Harness, session: &str) -> serde_json::Value {
    let resp = h
        .app
        .clone()
        .oneshot(authed("GET", "/v1/connections/gh/preflight", session))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    serde_json::from_str(&body_bytes(resp).await).unwrap()
}

/// The `(kind, name)` pairs the preflight reports as missing.
fn gaps(report: &serde_json::Value) -> Vec<(String, String)> {
    report["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["kind"].as_str().unwrap().to_string(),
                g["name"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn preflight_passes_a_fully_configured_app() {
    let h = harness(
        &[("acme", "web")],
        Arc::new(FakeForge::new().with_capabilities(HEALTHY_PERMISSIONS, HEALTHY_EVENTS)),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;

    let report = preflight(&h, &root).await;
    assert_eq!(report["status"], "ok");
    assert_eq!(report["checked"], true);
    assert!(report["unavailable_reason"].is_null());
    assert_eq!(gaps(&report), Vec::<(String, String)>::new());
    // What it saw is reported too, so an operator can compare against the App
    // settings page — as names and levels, never as anything that authenticates.
    assert!(report["subscribed_events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e == "push"));
    assert!(report["granted_permissions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "statuses" && p["level"] == "write"));

    // The list points at the check rather than performing it — a live forge
    // round-trip per row on every render of Settings is not a health readout.
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
    assert_eq!(conns[0]["supports_preflight"], true);
}

#[tokio::test]
async fn preflight_names_the_trigger_events_an_unsubscribed_app_is_missing() {
    // The App is granted everything and subscribed to NOTHING. Every other
    // signal in the product says healthy: the installation registered itself,
    // the credential resolves, /v1/repos lists the Project. No push will ever
    // start a run.
    let h = harness(
        &[("acme", "web")],
        Arc::new(FakeForge::new().with_capabilities(HEALTHY_PERMISSIONS, &[])),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;

    let report = preflight(&h, &root).await;
    assert_eq!(report["status"], "degraded");
    assert_eq!(report["checked"], true);
    let missing = report["missing"].as_array().unwrap();
    let required_events: Vec<&str> = missing
        .iter()
        .filter(|g| g["severity"] == "required")
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        required_events,
        vec!["push", "pull_request"],
        "the two events without which nothing ever triggers"
    );
    assert!(
        missing.iter().all(|g| g["kind"] == "event"),
        "permissions are fine — only the subscription is not"
    );
    // Each gap says what silently breaks, not just what is absent.
    let push = missing.iter().find(|g| g["name"] == "push").unwrap();
    assert!(
        push["why"].as_str().unwrap().contains("no push starts a run"),
        "{push}"
    );
}

#[tokio::test]
async fn preflight_reports_an_app_that_cannot_post_statuses() {
    // Same App, minus `statuses`. Runs will go green and the forge will never
    // show a check — the 403 happens deep in the status pipeline where nobody
    // is looking.
    let without_statuses: Vec<(&str, &str)> = HEALTHY_PERMISSIONS
        .iter()
        .copied()
        .filter(|(name, _)| *name != "statuses")
        .collect();
    let h = harness(
        &[("acme", "web")],
        Arc::new(FakeForge::new().with_capabilities(&without_statuses, HEALTHY_EVENTS)),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;

    let report = preflight(&h, &root).await;
    assert_eq!(report["status"], "degraded");
    assert_eq!(
        gaps(&report),
        vec![("permission".to_string(), "statuses".to_string())]
    );
    assert_eq!(report["missing"][0]["level"], "write");
    assert_eq!(report["missing"][0]["severity"], "required");
}

#[tokio::test]
async fn preflight_says_unknown_rather_than_healthy_when_it_cannot_look() {
    // The default FakeForge cannot introspect — the Forgejo/fixed-token
    // situation. "I could not check" must never render as "you are fine", and
    // the requirement list is still worth showing so the operator knows what to
    // configure by hand.
    let h = harness(&[("acme", "web")], Arc::new(FakeForge::new()), true).await;
    let root = login(&h.app, "root-code").await;

    let report = preflight(&h, &root).await;
    assert_eq!(report["status"], "unknown");
    assert_eq!(report["checked"], false);
    assert!(report["unavailable_reason"]
        .as_str()
        .unwrap()
        .contains("cannot report"));
    assert_eq!(gaps(&report), Vec::<(String, String)>::new());
    let required: Vec<&str> = report["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(required.contains(&"statuses") && required.contains(&"push"), "{required:?}");
}

#[tokio::test]
async fn preflight_never_returns_credential_material_and_needs_administer() {
    let h = harness(
        &[("acme", "web")],
        Arc::new(FakeForge::new().with_capabilities(HEALTHY_PERMISSIONS, HEALTHY_EVENTS)),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;

    let raw = body_bytes(
        h.app
            .clone()
            .oneshot(authed("GET", "/v1/connections/gh/preflight", &root))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        !raw.contains("super-secret-pem") && !raw.contains("BEGIN PRIVATE KEY"),
        "credential material leaked into the preflight payload: {raw}"
    );

    // Org-level act, same gate as the rest of the surface.
    let viewer = login(&h.app, "viewer-code").await;
    let resp = h
        .app
        .clone()
        .oneshot(authed("GET", "/v1/connections/gh/preflight", &viewer))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // An unknown connection is a 404, not an "unknown" report.
    let resp = h
        .app
        .clone()
        .oneshot(authed("GET", "/v1/connections/nope/preflight", &root))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn preflight_of_a_connection_whose_credential_is_gone_says_so_without_asking_the_forge() {
    // No credential = nothing to authenticate with. Reporting the App as
    // healthy because a fake answered would be the exact lie this endpoint
    // exists to stop.
    let h = harness(
        &[("acme", "web")],
        Arc::new(FakeForge::new().with_capabilities(HEALTHY_PERMISSIONS, HEALTHY_EVENTS)),
        false,
    )
    .await;
    let root = login(&h.app, "root-code").await;

    let report = preflight(&h, &root).await;
    assert_eq!(report["status"], "unknown");
    assert!(report["unavailable_reason"]
        .as_str()
        .unwrap()
        .contains("does not resolve"));
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
    let h = harness(&[("acme", "web")], Arc::new(FakeForge::new()), true).await;
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
    let conn =
        h.db.get_connection(&id)
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
    let h = harness(&[], Arc::new(FakeForge::new()), true).await;
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
    let h = harness(&[], Arc::new(FakeForge::new()), true).await;
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
    assert_eq!(
        h.db.list_connections().await.unwrap().len(),
        1,
        "nothing stored"
    );
}

#[tokio::test]
async fn a_second_connection_to_the_same_host_is_refused() {
    // Two rows for one host would each carry their own credential and each claim
    // to serve it — the shape a double-submitted form produces.
    let h = harness(&[], Arc::new(FakeForge::new()), true).await;
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
    let h = harness(&[], Arc::new(FakeForge::new()), true).await;
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
    let h = harness(&[("acme", "web")], Arc::new(FakeForge::new()), true).await;
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
    assert!(
        msg.contains("acme/web"),
        "the refusal names what it would delete: {msg}"
    );
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
    let h = harness(&[], Arc::new(FakeForge::new()), true).await;
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

// ---------------------------------------------------------------------------
// Repo binding → Project onboarding + webhook registration (ADR-0060 slice 5).
//
// The load-bearing fact: there is no `projects` table. A Project **is** a
// `forge_repos` binding (ADR-0046), so binding a repo is not a step towards
// onboarding — it *is* the onboarding. Until this endpoint existed, only the
// GitHub `installation` webhook ever called `bind_repo`, which is exactly why
// Forgejo shipped in ADR-0046 and remained unusable.
// ---------------------------------------------------------------------------

/// A `.scarab/ci.yaml` that runs one step on any push — the config the Forgejo
/// repo serves, so a delivery has something to trigger.
const CI_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

/// Create a Forgejo connection and return `(id, credential_ref)`.
async fn add_forgejo(h: &Harness, session: &str, base_url: &str) -> (String, String) {
    let resp = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/v1/connections",
            session,
            create_forgejo(base_url),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: serde_json::Value = serde_json::from_str(&body_bytes(resp).await).unwrap();
    (
        created["id"].as_str().unwrap().to_string(),
        created["credential_ref"].as_str().unwrap().to_string(),
    )
}

/// Bind `owner/name` on `id`, returning the raw response body.
async fn bind(
    h: &Harness,
    session: &str,
    id: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    h.app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/v1/connections/{id}/repos"),
            session,
            body,
        ))
        .await
        .unwrap()
}

/// A signed Forgejo push delivery for `owner/name`.
fn forgejo_push(owner: &str, name: &str, delivery: &str) -> Request<Body> {
    let body = serde_json::to_vec(&serde_json::json!({
        "ref": "refs/heads/main",
        "after": "c0ffee1",
        "head_commit": { "message": "feat: onboard via Scarab" },
        // Forgejo's `username` spelling — the variant GitHub never sends.
        "repository": { "name": name, "owner": { "username": owner } },
        "sender": { "username": "pusher" }
    }))
    .unwrap();
    let sig = scarab_forge_forgejo::sign_hex(FORGEJO_WEBHOOK_SECRET, &body);
    Request::builder()
        .method("POST")
        .uri("/webhooks/forgejo")
        .header("content-type", "application/json")
        .header("x-forgejo-event", "push")
        .header("x-forgejo-delivery", delivery)
        .header("x-forgejo-signature", sig)
        .body(Body::from(body))
        .unwrap()
}

/// **The slice's whole claim, end to end**: an admin adds a Forgejo connection,
/// picks a repo off the list the credential reaches, binds it — and a push to that
/// repo starts a Run. This is the path GitHub has had since ADR-0046 and Forgejo
/// never did, because nothing but the `installation` webhook called `bind_repo`.
#[tokio::test]
async fn onboarding_a_forgejo_repo_creates_a_project_whose_pushes_run() {
    let h = harness(
        &[],
        Arc::new(
            FakeForge::new()
                .with_file(".scarab/ci.yaml", CI_YAML)
                .with_accessible_repos(&[("acme", "web"), ("acme", "docs")]),
        ),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;
    let (id, credential_ref) = add_forgejo(&h, &root, "https://git.acme.test").await;

    // 1. The pick-list: what the credential reaches, and what is already governed.
    //    An admin choosing from this cannot mistype `owner/name`.
    let available_raw = body_bytes(
        h.app
            .clone()
            .oneshot(authed(
                "GET",
                &format!("/v1/connections/{id}/available-repos"),
                &root,
            ))
            .await
            .unwrap(),
    )
    .await;
    let available: serde_json::Value = serde_json::from_str(&available_raw).unwrap();
    assert_eq!(
        available,
        serde_json::json!([
            { "owner": "acme", "name": "docs", "bound": false },
            { "owner": "acme", "name": "web", "bound": false },
        ]),
        "sorted, nothing bound yet"
    );

    // 2. Bind — which CREATES the Project (there is no projects table).
    let bind_raw = body_bytes(
        bind(
            &h,
            &root,
            &id,
            serde_json::json!({ "owner": "acme", "name": "web" }),
        )
        .await,
    )
    .await;
    let bound: serde_json::Value = serde_json::from_str(&bind_raw).unwrap();
    assert_eq!(bound["org"], "acme");
    assert_eq!(bound["project"], "web");
    assert_eq!(
        bound["webhook_registered"], true,
        "registration is part of binding, not a second chore"
    );
    assert!(bound["webhook_error"].is_null());

    // The hook points at THIS forge's ingest endpoint (ADR-0046: one endpoint per
    // forge, each with its own verification secret — no payload sniffing).
    assert_eq!(
        h.forge.webhooks(),
        vec![(
            RepoRef {
                owner: "acme".into(),
                name: "web".into()
            },
            "http://localhost:8080/webhooks/forgejo".to_string()
        )]
    );

    // 3. The Project is real: it appears on the Repos page and resolves to its
    //    serving connection.
    let repos = body_bytes(
        h.app
            .clone()
            .oneshot(authed("GET", "/v1/repos", &root))
            .await
            .unwrap(),
    )
    .await;
    assert!(repos.contains("\"project\":\"web\""), "{repos}");
    // Its repo deep-link is the Forgejo host, not github.com.
    assert!(repos.contains("https://git.acme.test/acme/web"), "{repos}");
    let resolved =
        h.db.resolve(&RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        })
        .await
        .unwrap()
        .expect("the binding IS the Project");
    assert_eq!(resolved.connection.id, id);

    // 4. The payoff: a signed push on that repo starts a Run, attributed to the
    //    Project the bind created.
    let resp = h
        .app
        .clone()
        .oneshot(forgejo_push("acme", "web", "fj-onboard-1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let ingested: serde_json::Value = serde_json::from_str(&body_bytes(resp).await).unwrap();
    assert_eq!(ingested["trigger"], "push");
    let run_id = ingested["run_ids"][0].as_str().unwrap().to_string();
    assert!(h
        .db
        .run_status(&scarab_engine::RunId(run_id.clone()))
        .await
        .unwrap()
        .is_some());
    let project_runs = body_bytes(
        h.app
            .clone()
            .oneshot(authed("GET", "/v1/repos/acme/web/runs", &root))
            .await
            .unwrap(),
    )
    .await;
    let listed: serde_json::Value = serde_json::from_str(&project_runs).unwrap();
    assert_eq!(
        listed["runs"][0]["id"].as_str(),
        Some(run_id.as_str()),
        "the run is listed under the Project the bind created: {project_runs}"
    );
    assert_eq!(listed["runs"][0]["org"], "acme");
    assert_eq!(listed["runs"][0]["project"], "web");
    assert_eq!(listed["runs"][0]["trigger_kind"], "push");

    // 5. Not one of these responses carries the credential.
    for (what, raw) in [
        ("available-repos", &available_raw),
        ("bind", &bind_raw),
        ("repos", &repos),
        ("project runs", &project_runs),
    ] {
        assert!(
            !raw.contains(FORGEJO_TOKEN),
            "the credential leaked into the {what} response: {raw}"
        );
    }
    // The material is still exactly where the write-through put it — nothing on
    // the bind path moved or copied it.
    assert_eq!(
        h.stored_credential(&credential_ref).await.as_deref(),
        Some(FORGEJO_TOKEN.as_bytes())
    );
}

#[tokio::test]
async fn unbinding_removes_the_project() {
    let h = harness(
        &[],
        Arc::new(FakeForge::new().with_accessible_repos(&[("acme", "web")])),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;
    let (id, _) = add_forgejo(&h, &root, "https://git.acme.test").await;
    bind(
        &h,
        &root,
        &id,
        serde_json::json!({ "owner": "acme", "name": "web" }),
    )
    .await;

    // Now bound, the pick-list says so — so the form can show "already added"
    // instead of offering a re-bind that would re-home a live Project.
    let available: serde_json::Value = serde_json::from_str(
        &body_bytes(
            h.app
                .clone()
                .oneshot(authed(
                    "GET",
                    &format!("/v1/connections/{id}/available-repos"),
                    &root,
                ))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert_eq!(available[0]["bound"], true);

    let resp = h
        .app
        .clone()
        .oneshot(authed(
            "DELETE",
            &format!("/v1/connections/{id}/repos/acme/web"),
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
    // Unbinding again is a 404, not a silent success: an unbind aimed at the
    // wrong connection must not read as "already gone".
    let resp = h
        .app
        .clone()
        .oneshot(authed(
            "DELETE",
            &format!("/v1/connections/{id}/repos/acme/web"),
            &root,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A hook the forge refuses does NOT undo the binding — and the failure is
/// reported, not swallowed. Rolling back would delete a Project an admin just
/// asked for because a remote system was briefly unreachable; hiding the error
/// would leave a Project that silently never builds.
#[tokio::test]
async fn a_failed_webhook_leaves_the_project_and_says_so() {
    let h = harness(
        &[],
        Arc::new(
            FakeForge::new()
                .with_accessible_repos(&[("acme", "web")])
                .failing_webhook("403: token lacks write:repository_hook"),
        ),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;
    let (id, _) = add_forgejo(&h, &root, "https://git.acme.test").await;

    let result: serde_json::Value = serde_json::from_str(
        &body_bytes(
            bind(
                &h,
                &root,
                &id,
                serde_json::json!({ "owner": "acme", "name": "web" }),
            )
            .await,
        )
        .await,
    )
    .unwrap();
    assert_eq!(result["webhook_registered"], false);
    assert!(
        result["webhook_error"]
            .as_str()
            .unwrap()
            .contains("write:repository_hook"),
        "the forge's own reason reaches the admin: {result}"
    );
    // The governance fact stands: the Project exists and can be configured while
    // the token gets fixed.
    assert!(h
        .db
        .resolve(&RepoRef {
            owner: "acme".into(),
            name: "web".into()
        })
        .await
        .unwrap()
        .is_some());

    // …and the retry endpoint is the way out. It surfaces the forge's refusal as
    // a 4xx rather than pretending.
    let resp = h
        .app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/connections/{id}/repos/acme/web/webhook"),
            &root,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Registering a hook for a repo this connection does NOT govern is a 404:
    // deliveries for an unbound repo resolve to nothing.
    let resp = h
        .app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/connections/{id}/repos/acme/nope/webhook"),
            &root,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Binding a repo another connection already governs is a conflict, not a move.
/// `bind_repo` upserts, so accepting it would silently re-home a live Project —
/// its runs, Environments and secrets — onto a different forge account.
#[tokio::test]
async fn a_repo_bound_elsewhere_cannot_be_rebound() {
    let h = harness(
        &[("acme", "web")],
        Arc::new(FakeForge::new().with_accessible_repos(&[("acme", "web")])),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;
    let (id, _) = add_forgejo(&h, &root, "https://git.acme.test").await;

    let resp = bind(
        &h,
        &root,
        &id,
        serde_json::json!({ "owner": "acme", "name": "web" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let msg = body_bytes(resp).await;
    assert!(msg.contains("gh"), "the refusal names the owner: {msg}");
    // Still the GitHub installation's Project — nothing was re-homed.
    assert_eq!(
        h.db.resolve(&RepoRef {
            owner: "acme".into(),
            name: "web".into()
        })
        .await
        .unwrap()
        .unwrap()
        .connection
        .id,
        "gh"
    );

    // Re-binding to the SAME connection is idempotent, though — a pick-list may
    // legitimately be double-clicked.
    let repo = RepoRef {
        owner: "acme".into(),
        name: "api".into(),
    };
    for _ in 0..2 {
        let resp = bind(
            &h,
            &root,
            &id,
            serde_json::json!({ "owner": "acme", "name": "api", "register_webhook": false }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    assert_eq!(
        h.db.resolve(&repo).await.unwrap().unwrap().connection.id,
        id
    );
    // `register_webhook: false` was honored — no hook was created.
    assert!(h.forge.webhooks().is_empty());
}

#[tokio::test]
async fn binding_endpoints_need_administer_on_the_org() {
    let h = harness(
        &[("acme", "web")],
        Arc::new(FakeForge::new().with_accessible_repos(&[("acme", "web")])),
        true,
    )
    .await;
    let viewer = login(&h.app, "viewer-code").await;

    let resp = bind(
        &h,
        &viewer,
        "gh",
        serde_json::json!({ "owner": "acme", "name": "api" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    for (method, uri) in [
        ("GET", "/v1/connections/gh/available-repos"),
        ("DELETE", "/v1/connections/gh/repos/acme/web"),
        ("POST", "/v1/connections/gh/repos/acme/web/webhook"),
    ] {
        let resp = h
            .app
            .clone()
            .oneshot(authed(method, uri, &viewer))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
    }
    // Nothing changed: acme/web is still the GitHub installation's Project.
    assert!(h
        .db
        .resolve(&RepoRef {
            owner: "acme".into(),
            name: "web".into()
        })
        .await
        .unwrap()
        .is_some());
}

/// A Forgejo connection now advertises re-sync, because its adapter can
/// enumerate — the same `/user/repos` capability the pick-list rides on.
#[tokio::test]
async fn a_forgejo_connection_offers_resync_too() {
    let h = harness(
        &[],
        Arc::new(FakeForge::new().with_accessible_repos(&[("acme", "web")])),
        true,
    )
    .await;
    let root = login(&h.app, "root-code").await;
    let (id, _) = add_forgejo(&h, &root, "https://git.acme.test").await;

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
    let fj = conns
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == serde_json::json!(id))
        .unwrap();
    assert_eq!(fj["supports_resync"], true);

    // And it works: re-sync binds what the credential reaches.
    let resp = h
        .app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/connections/{id}/resync"),
            &root,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body_bytes(resp).await).unwrap();
    assert_eq!(result["bound"], serde_json::json!(["acme/web"]));
}
