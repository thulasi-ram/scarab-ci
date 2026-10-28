//! Org-level Environments over HTTP (ADR-0071, MMP step 1): the admin and
//! read API on a *real* Postgres store, and who may call it.
//!
//! The engine `Db` is the in-memory one; nothing here asks it for anything.
//! Each test gets its own database (`fresh_db`). Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset (except the unwired-store test).

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{Binding, BindingOrigin, Principal, RbacStore, Role, Scope};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, InMemoryDb, InMemoryObjectStore, InMemoryRbac, InMemorySessions,
};

const CLOCK_START_MS: i64 = 1_000_000;

const MINUTE_MS: i64 = 60_000;

fn app_state(db: Arc<InMemoryDb>, clock: Arc<FakeClock>) -> AppState {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    AppState::new(db, clock, logs)
}

/// A router with the store wired and authz off (every caller an owner).
fn open_app(pg: Arc<PostgresDb>, clock: Arc<FakeClock>) -> axum::Router {
    router(app_state(Arc::new(InMemoryDb::new()), clock).with_org_environments(pg))
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    session: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(session) = session {
        req = req.header("authorization", format!("Bearer {session}"));
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

#[tokio::test]
async fn create_read_replace_and_delete_round_trip() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let clock = Arc::new(FakeClock::new(CLOCK_START_MS));
    let app = open_app(pg, clock.clone());

    let (status, created) = call(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/prod",
        None,
        Some(json!({ "description": "the one prod" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(
        created,
        json!({
            "org": "acme",
            "name": "prod",
            "description": "the one prod",
            // ADR-0071 §1-2: a new Environment starts with the permissive
            // baseline and eligibility CLOSED — an empty list admits no
            // Project until an org admin names some. Pinned here because the
            // whole-Environment PUT is how an Environment is born.
            "protection": {
                "approvers": [],
                "allowed_refs": [],
                "wait_timer_s": 0,
                "require_reason": false
            },
            "eligibility": { "all_projects": false, "projects": [] },
            "created_at": CLOCK_START_MS,
            "updated_at": CLOCK_START_MS,
            // Authz is off here: the dev principal.
            "updated_by": "anonymous"
        })
    );
    let (status, got) = call(&app, "GET", "/v1/orgs/acme/environments/prod", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, created);

    // An omitted description takes its default on a replace — a full
    // replace, not a merge — and created_at stays.
    clock.advance(MINUTE_MS);
    let (status, replaced) = call(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/prod",
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["description"], json!(""));
    assert_eq!(replaced["created_at"], json!(CLOCK_START_MS));
    assert_eq!(replaced["updated_at"], json!(CLOCK_START_MS + MINUTE_MS));

    call(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/staging",
        None,
        Some(json!({})),
    )
    .await;
    let (status, list) = call(&app, "GET", "/v1/orgs/acme/environments", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["prod", "staging"]);
    let (_, other_org) = call(&app, "GET", "/v1/orgs/initech/environments", None, None).await;
    assert_eq!(other_org, json!([]));

    let (status, _) = call(
        &app,
        "DELETE",
        "/v1/orgs/acme/environments/prod",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, "GET", "/v1/orgs/acme/environments/prod", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        "DELETE",
        "/v1/orgs/acme/environments/prod",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing left to delete");

    tdb.cleanup().await;
}

#[tokio::test]
async fn invalid_writes_are_refused_and_store_nothing() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = open_app(pg, Arc::new(FakeClock::new(CLOCK_START_MS)));

    for uri in [
        // A control character in the name.
        "/v1/orgs/acme/environments/prod%09eu",
        // A dot segment for a name: a client would normalise it away.
        "/v1/orgs/acme/environments/%2E%2E",
    ] {
        let (status, body_out) = call(&app, "PUT", uri, None, Some(json!({}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} -> {body_out}");
    }
    // A misspelt field is refused rather than silently ignored; so are the
    // fields ADR-0074 removed with the Deployment record.
    for body in [
        json!({ "descripton": "typo" }),
        json!({ "deployment_deadline_ms": 5 * MINUTE_MS }),
        json!({ "deployment_sources": [] }),
    ] {
        let (status, _) = call(
            &app,
            "PUT",
            "/v1/orgs/acme/environments/prod",
            None,
            Some(body.clone()),
        )
        .await;
        assert!(status.is_client_error(), "{body} -> {status}");
    }
    let (_, list) = call(&app, "GET", "/v1/orgs/acme/environments", None, None).await;
    assert_eq!(list, json!([]));

    tdb.cleanup().await;
}

/// A name a pipeline can deploy to — spaces, a `/` — is registrable and
/// addressable: the router matches on the encoded path, so `%2F` stays inside
/// the one `{env}` segment and arrives decoded.
#[tokio::test]
async fn names_with_spaces_and_slashes_round_trip() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = open_app(pg, Arc::new(FakeClock::new(CLOCK_START_MS)));

    for (encoded, name) in [
        ("Production%20EU", "Production EU"),
        ("prod%2Feu", "prod/eu"),
    ] {
        let uri = format!("/v1/orgs/acme/environments/{encoded}");
        let (status, put) = call(&app, "PUT", &uri, None, Some(json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{name}: {put}");
        assert_eq!(put["name"], json!(name));
        let (status, got) = call(&app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "{name}");
        assert_eq!(got, put);
        let (status, _) = call(&app, "DELETE", &uri, None, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{name}");
        let (status, _) = call(&app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{name}");
    }

    tdb.cleanup().await;
}

#[tokio::test]
async fn writes_need_org_administer_and_reads_need_org_read() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();

    let scoped = |subject: &str| Principal {
        subject: subject.into(),
        display_name: None,
        roles: vec![],
    };
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential("ada-code", scoped("ada"))
            .with_credential("mel-code", scoped("mel"))
            .with_credential("eve-code", scoped("eve")),
    );
    let rbac = Arc::new(InMemoryRbac::new());
    for (subject, org, role) in [
        ("ada", "acme", Role::Admin),
        ("mel", "acme", Role::Member),
        ("eve", "evil", Role::Admin),
    ] {
        rbac.grant(
            &Binding {
                subject: subject.into(),
                scope: Scope::Org(org.into()),
                role,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    }
    let app = router(
        app_state(
            Arc::new(InMemoryDb::new()),
            Arc::new(FakeClock::new(CLOCK_START_MS)),
        )
        .with_auth(auth, Arc::new(InMemorySessions::new()))
        .with_rbac(rbac)
        .with_org_environments(pg),
    );
    let session = |credential: &'static str| {
        let app = app.clone();
        async move {
            let (status, body) = call(
                &app,
                "POST",
                "/v1/auth/login",
                None,
                Some(json!({ "credential": credential })),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            body["session"].as_str().unwrap().to_string()
        }
    };
    let (ada, mel, eve) = (
        session("ada-code").await,
        session("mel-code").await,
        session("eve-code").await,
    );
    let put_body = || Some(json!({ "description": "prod" }));
    let prod = "/v1/orgs/acme/environments/prod";

    let (status, created) = call(&app, "PUT", prod, Some(&ada), put_body()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        created["updated_by"],
        json!("ada"),
        "the writer is recorded"
    );

    for (who, session) in [("mel", &mel), ("eve", &eve)] {
        let (status, _) = call(&app, "PUT", prod, Some(session), put_body()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who} PUT");
        let (status, _) = call(&app, "DELETE", prod, Some(session), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who} DELETE");
    }
    for uri in ["/v1/orgs/acme/environments", prod] {
        let (status, _) = call(&app, "GET", uri, Some(&mel), None).await;
        assert_eq!(status, StatusCode::OK, "member reads {uri}");
        let (status, _) = call(&app, "GET", uri, Some(&eve), None).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "another org's admin reads {uri}"
        );
        let (status, _) = call(&app, "GET", uri, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "anonymous reads {uri}");
    }
    let (status, _) = call(&app, "DELETE", prod, Some(&ada), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    tdb.cleanup().await;
}

/// No store wired → the endpoints do not exist here (404). Needs no
/// database.
#[tokio::test]
async fn unwired_store_is_404() {
    let app = router(app_state(
        Arc::new(InMemoryDb::new()),
        Arc::new(FakeClock::new(0)),
    ));
    for (method, uri, body) in [
        ("GET", "/v1/orgs/acme/environments", None),
        ("GET", "/v1/orgs/acme/environments/prod", None),
        ("PUT", "/v1/orgs/acme/environments/prod", Some(json!({}))),
        ("DELETE", "/v1/orgs/acme/environments/prod", None),
    ] {
        let (status, _) = call(&app, method, uri, None, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
}
