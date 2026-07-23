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
use scarab_identity::{Binding, BindingOrigin, Principal, RbacStore, Role, Scope};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeForge, InMemoryDb, InMemoryObjectStore, InMemoryRbac,
    InMemorySessions,
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
}

fn harness() -> Harness {
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
    let app = router(
        AppState::new(db.clone(), clock, logs)
            .with_auth(auth, Arc::new(InMemorySessions::new()))
            .with_rbac(rbac.clone())
            .with_forge(Arc::new(FakeForge::new())),
    );
    Harness { app, db, rbac }
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

    // Import seeds carol from the forge (FakeForge grants admin) — but a
    // native revoke tombstone must survive a later import.
    let import = || {
        Request::builder()
            .method("POST")
            .uri("/v1/repos/acme/app/bindings/import")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {root}"))
            .body(Body::from(
                serde_json::json!({ "subjects": ["carol"] }).to_string(),
            ))
            .unwrap()
    };
    let resp = h.app.clone().oneshot(import()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let imported = body_json(resp).await;
    assert_eq!(imported[0]["subject"], "carol");
    assert_eq!(imported[0]["role"], "admin");
    let app_scope = Scope::Project {
        org: "acme".into(),
        name: "app".into(),
    };
    assert_eq!(
        h.rbac.role_of("carol", &app_scope).await.unwrap(),
        Some(Role::Admin)
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
    // …and a re-import does NOT resurrect the grant (native override wins).
    let resp = h.app.clone().oneshot(import()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
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
