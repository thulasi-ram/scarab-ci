//! Scoped RBAC + tenancy (ADR-0049 C2), hermetic: runs stamped with their
//! owning tenant are readable/writable only by principals whose bindings
//! grant a role in that Org/Project (Org inherits down); cross-tenant access
//! is denied; list_runs filters to the caller's tenants; the forge import
//! seeds bindings without clobbering native decisions.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{Db, RunId};
use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_identity::{
    Binding, BindingOrigin, IdentityStore, Principal, ProviderIdentity, RbacStore, Role, Scope,
};
use scarab_server::config::OAuthConfig;
use scarab_server::oauth::OAuthAuthenticator;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeForge, InMemoryDb, InMemoryIdentities, InMemoryObjectStore,
    InMemoryRbac, InMemorySessions,
};

/// A principal with NO global roles — only scoped bindings decide.
fn scoped_principal(subject: &str) -> Principal {
    Principal {
        subject: subject.into(),
        display_name: None,
        roles: vec![],
    }
}

struct Harness {
    app: axum::Router,
    db: Arc<InMemoryDb>,
    rbac: Arc<InMemoryRbac>,
    identities: Arc<InMemoryIdentities>,
    forge: Arc<FakeForge>,
}

/// The login provider whose accounts ARE the GitHub forge (ADR-0069 §5) —
/// the legacy bare-id one. Never asked to exchange anything here: sign-in is
/// the `FakeAuthenticator`'s job; this only answers "which provider's
/// identities does an import from `https://api.github.com` resolve through".
fn github_provider(forge_url: Option<&str>) -> Arc<OAuthAuthenticator> {
    Arc::new(OAuthAuthenticator::new(OAuthConfig {
        client_id: "cid".into(),
        client_secret: "s".into(),
        authorize_url: "https://github.invalid/authorize".into(),
        token_url: "https://github.invalid/token".into(),
        userinfo_url: "https://github.invalid/user".into(),
        oidc_issuer: None,
        scopes: String::new(),
        owners: vec![],
        provider_id: "default".into(),
        unqualified_ids: true,
        name: "GitHub".into(),
        forge_url: forge_url.map(String::from),
    }))
}

/// A forge account as an import (or a sign-in) presents it.
fn account(external_id: &str, login: &str) -> ProviderIdentity {
    ProviderIdentity {
        provider: "default".into(),
        external_id: external_id.into(),
        handle: login.into(),
        email: None,
        email_verified: false,
        display_name: None,
    }
}

fn harness() -> Harness {
    harness_with(Some("https://api.github.com"))
}

fn harness_with(forge_url: Option<&str>) -> Harness {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
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
            .with_credential("amy-code", scoped_principal("amy"))
            .with_credential("eve-code", scoped_principal("eve")),
    );
    let rbac = Arc::new(InMemoryRbac::new());
    let identities = Arc::new(InMemoryIdentities::new());
    let forge = Arc::new(FakeForge::new());
    // acme/app is served by a GitHub connection — the forge an import asks.
    futures::executor::block_on(async {
        db.put_connection(&ForgeConnection {
            id: "gh".into(),
            kind: ForgeKind::GitHub,
            base_url: "https://api.github.com".into(),
            credential_ref: "github-app".into(),
        })
        .await
        .unwrap();
        let repo = RepoRef {
            owner: "acme".into(),
            name: "app".into(),
        };
        db.bind_repo("gh", &repo, "acme", "app").await.unwrap();
        // The scoped principals have signed in, so a native grant may name them.
        for (id, who) in [("1", "amy"), ("2", "eve")] {
            identities.sign_in(&account(id, who), who, 1).await.unwrap();
        }
    });
    let app = router(
        AppState::new(db.clone(), clock, logs)
            .with_auth(auth, Arc::new(InMemorySessions::new()))
            .with_rbac(rbac.clone())
            .with_identities(identities.clone())
            .with_forge(forge.clone())
            .with_forge_connections(db.clone())
            .with_oauth_login(github_provider(forge_url)),
    );
    Harness {
        app,
        db,
        rbac,
        identities,
        forge,
    }
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

/// Create a run as the global owner, then stamp its tenant.
async fn seed_run(h: &Harness, root: &str, org: &str, project: &str) -> String {
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {root}"))
                .body(Body::from(
                    serde_json::json!({
                        "pipeline": {
                            "ir_version": 1,
                            "steps": [{ "id": "b", "image": "busybox", "command": ["true"] }]
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = body_json(resp).await["id"].as_str().unwrap().to_string();
    h.db.set_run_tenant(&RunId(id.clone()), org, project)
        .await
        .unwrap();
    id
}

fn get(uri: &str, session: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {session}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn run_reads_are_tenant_scoped_and_cross_tenant_is_denied() {
    let h = harness();
    let root = login(&h.app, "root-code").await;
    let acme_run = seed_run(&h, &root, "acme", "app").await;
    let evil_run = seed_run(&h, &root, "evil", "app").await;

    // amy is a Viewer of org acme (org role inherits to the project).
    h.rbac
        .grant(
            &Binding {
                subject: "amy".into(),
                scope: Scope::Org("acme".into()),
                role: Role::Viewer,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    let amy = login(&h.app, "amy-code").await;

    // amy reads acme's run — and every read surface of it.
    for uri in [
        format!("/v1/runs/{acme_run}"),
        format!("/v1/runs/{acme_run}/events"),
        format!("/v1/runs/{acme_run}/logs"),
    ] {
        let resp = h.app.clone().oneshot(get(&uri, &amy)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    }

    // CROSS-TENANT DENIAL: the other org's run is forbidden for amy.
    let resp = h
        .app
        .clone()
        .oneshot(get(&format!("/v1/runs/{evil_run}"), &amy))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // eve (no bindings at all) is denied both.
    let eve = login(&h.app, "eve-code").await;
    for run in [&acme_run, &evil_run] {
        let resp = h
            .app
            .clone()
            .oneshot(get(&format!("/v1/runs/{run}"), &eve))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    // The global owner still sees everything.
    let resp = h
        .app
        .clone()
        .oneshot(get(&format!("/v1/runs/{evil_run}"), &root))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn list_runs_filters_to_the_callers_tenants() {
    let h = harness();
    let root = login(&h.app, "root-code").await;
    let acme_run = seed_run(&h, &root, "acme", "app").await;
    let evil_run = seed_run(&h, &root, "evil", "app").await;

    h.rbac
        .grant(
            &Binding {
                subject: "amy".into(),
                scope: Scope::Project {
                    org: "acme".into(),
                    name: "app".into(),
                },
                role: Role::Viewer,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    let amy = login(&h.app, "amy-code").await;

    let v = body_json(h.app.clone().oneshot(get("/v1/runs", &amy)).await.unwrap()).await;
    let ids: Vec<String> = v["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        ids.contains(&acme_run),
        "amy sees her tenant's run: {ids:?}"
    );
    assert!(
        !ids.contains(&evil_run),
        "the other tenant's run is filtered out: {ids:?}"
    );

    // The global owner sees both.
    let v = body_json(h.app.clone().oneshot(get("/v1/runs", &root)).await.unwrap()).await;
    let ids: Vec<String> = v["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&acme_run) && ids.contains(&evil_run));
}

#[tokio::test]
async fn scoped_writes_respect_role_and_tenant() {
    let h = harness();
    let root = login(&h.app, "root-code").await;
    let acme_run = seed_run(&h, &root, "acme", "app").await;
    let evil_run = seed_run(&h, &root, "evil", "app").await;

    // amy is a Member (Write) of acme.
    h.rbac
        .grant(
            &Binding {
                subject: "amy".into(),
                scope: Scope::Org("acme".into()),
                role: Role::Member,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    let amy = login(&h.app, "amy-code").await;

    let rerun = |run: &str, session: &str| {
        Request::builder()
            .method("POST")
            .uri(format!("/v1/runs/{run}/steps/b/rerun"))
            .header("authorization", format!("Bearer {session}"))
            .body(Body::empty())
            .unwrap()
    };
    // Write inside her tenant is allowed…
    let resp = h.app.clone().oneshot(rerun(&acme_run, &amy)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    // …a cross-tenant write is forbidden.
    let resp = h.app.clone().oneshot(rerun(&evil_run, &amy)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn bindings_api_grants_lists_revokes_and_imports() {
    let h = harness();
    let root = login(&h.app, "root-code").await;

    // Native grant via the API.
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/orgs/acme/bindings")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {root}"))
                .body(Body::from(
                    serde_json::json!({ "subject": "amy", "role": "member", "project": "app" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // ADR-0069: the import lists the forge's collaborators and resolves each
    // by its IMMUTABLE id. Nobody has signed in as carol or dan yet, so each
    // becomes an unclaimed Principal under the legacy provider's bare id.
    h.forge.set_collaborators(&[
        ("3003", "carol", "admin"),
        ("4004", "dan", "read"),
        ("5005", "nobody", "none"),
    ]);
    let import = |subjects: serde_json::Value| {
        Request::builder()
            .method("POST")
            .uri("/v1/repos/acme/app/bindings/import")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {root}"))
            .body(Body::from(
                serde_json::json!({ "subjects": subjects }).to_string(),
            ))
            .unwrap()
    };
    let resync = || import(serde_json::json!([]));
    let resp = h.app.clone().oneshot(resync()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let imported = body_json(resp).await;
    assert_eq!(
        imported,
        serde_json::json!([
            { "subject": "carol", "role": "admin", "project": "app" },
            { "subject": "dan", "role": "viewer", "project": "app" },
        ]),
        "no-access collaborators are not seeded"
    );
    let app_scope = Scope::Project {
        org: "acme".into(),
        name: "app".into(),
    };
    assert_eq!(
        h.rbac.role_of("carol", &app_scope).await.unwrap(),
        Some(Role::Admin)
    );
    let dan = h.identities.principal("dan").await.unwrap().unwrap();
    assert_eq!(dan.claimed_at, None, "imported, not yet signed into");
    // Signing in with THAT forge account claims it — same Principal.
    assert_eq!(
        h.identities
            .sign_in(&account("4004", "dan"), "dan", 9)
            .await
            .unwrap(),
        "dan"
    );

    // Native revoke via the API…
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/orgs/acme/bindings?subject=carol&project=app")
                .header("authorization", format!("Bearer {root}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(h.rbac.role_of("carol", &app_scope).await.unwrap(), None);
    // …and a re-sync does NOT resurrect the grant (native override wins).
    let resp = h.app.clone().oneshot(resync()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.rbac.role_of("carol", &app_scope).await.unwrap(), None);

    // A listing that fails part-way changes NOTHING — never a partial prune.
    h.forge.fail_collaborators("page 2: 403 rate limited");
    let resp = h.app.clone().oneshot(resync()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        h.rbac.role_of("dan", &app_scope).await.unwrap(),
        Some(Role::Viewer)
    );

    // dan leaves the forge: a re-sync removes the role it seeded. A FILTERED
    // import is no claim about anyone else, so it would not have.
    h.forge.set_collaborators(&[("3003", "carol", "admin")]);
    let resp = h
        .app
        .clone()
        .oneshot(import(serde_json::json!(["carol"])))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        h.rbac.role_of("dan", &app_scope).await.unwrap(),
        Some(Role::Viewer),
        "a filtered import never prunes"
    );
    let resp = h.app.clone().oneshot(resync()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.rbac.role_of("dan", &app_scope).await.unwrap(), None);
    // amy's NATIVE grant is not the forge's to remove.
    assert_eq!(
        h.rbac.role_of("amy", &app_scope).await.unwrap(),
        Some(Role::Member)
    );

    // A stranger takes over the released login `carol`: a different forge id,
    // so a different Principal — the role lands on `carol~2`, never on the
    // person whose grant was explicitly revoked as `carol`.
    h.forge.set_collaborators(&[("6006", "carol", "admin")]);
    let resp = h.app.clone().oneshot(resync()).await.unwrap();
    assert_eq!(body_json(resp).await[0]["subject"], "carol~2");
    assert_eq!(h.rbac.role_of("carol", &app_scope).await.unwrap(), None);

    // Listing shows the live bindings.
    let v = body_json(
        h.app
            .clone()
            .oneshot(get("/v1/orgs/acme/bindings", &root))
            .await
            .unwrap(),
    )
    .await;
    let subjects: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["subject"].as_str().unwrap())
        .collect();
    assert!(subjects.contains(&"amy"));
    assert!(
        !subjects.contains(&"carol"),
        "tombstoned carol is not listed"
    );

    // A non-admin cannot manage bindings.
    let amy = login(&h.app, "amy-code").await;
    let resp = h
        .app
        .clone()
        .oneshot(get("/v1/orgs/acme/bindings", &amy))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// An import through a forge that no login provider IS refuses loudly (409)
/// instead of seeding roles nobody could ever sign into — the generic-OIDC
/// gap of ADR-0069 problem (1), made visible.
#[tokio::test]
async fn an_import_with_no_provider_for_the_forge_is_a_409() {
    let h = harness_with(None);
    h.forge.set_collaborators(&[("3003", "carol", "admin")]);
    let root = login(&h.app, "root-code").await;
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/repos/acme/app/bindings/import")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {root}"))
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let msg = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(msg.contains("SCARAB_OAUTH_FORGE_URL"), "{msg}");
    assert!(h.identities.principal("carol").await.unwrap().is_none());
}
