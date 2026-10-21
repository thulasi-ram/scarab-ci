//! Org-level Environments over HTTP (ADR-0071, MMP step 1): the admin and
//! read API on a *real* Postgres store, who may call it, and the one thing
//! they change elsewhere — the EnvironmentDeploymentDeadline the Deployment
//! read fold stalls by.
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
use scarab_deployment::{
    DeploymentInput, DeploymentRequester, DeploymentStore, DeploymentTarget, DeploymentVersion,
    DEFAULT_DEPLOYMENT_DEADLINE_MS,
};
use scarab_engine::Clock;
use scarab_identity::{Binding, BindingOrigin, Principal, RbacStore, Role, Scope};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, InMemoryDb, InMemoryObjectStore, InMemoryRbac, InMemorySessions,
};

/// Where the server's FakeClock starts — not ahead of the database clock (the
/// Deployment store refuses a runaway `received_at`).
const CLOCK_START_MS: i64 = 1_000_000;

const MINUTE_MS: i64 = 60_000;

fn app_state(db: Arc<InMemoryDb>, clock: Arc<FakeClock>) -> AppState {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    AppState::new(db, clock, logs)
}

/// A router with both stores wired and authz off (every caller an owner).
fn open_app(pg: Arc<PostgresDb>, clock: Arc<FakeClock>) -> axum::Router {
    router(
        app_state(Arc::new(InMemoryDb::new()), clock)
            .with_deployments(pg.clone())
            .with_org_environments(pg),
    )
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

fn request(environment: &str, id: &str, version: &str) -> DeploymentInput {
    DeploymentInput::Request {
        target: DeploymentTarget {
            environment: environment.into(),
            source: "argocd".into(),
            id: id.into(),
        },
        version: DeploymentVersion(version.into()),
        requester: DeploymentRequester {
            run: "run-1".into(),
            step: "bump-gitops".into(),
        },
        effective_at: 0,
        at: 0,
    }
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
        Some(json!({
            "description": "the one prod",
            "deployment_deadline_ms": 5 * MINUTE_MS,
            "deployment_sources": [
                { "installation": "argocd-prod", "vendor_grouping": "prod" },
                { "installation": "argocd-eu", "vendor_grouping": "prod-eu" }
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(
        created,
        json!({
            "org": "acme",
            "name": "prod",
            "description": "the one prod",
            "deployment_deadline_ms": 5 * MINUTE_MS,
            "deployment_sources": [
                { "installation": "argocd-eu", "vendor_grouping": "prod-eu" },
                { "installation": "argocd-prod", "vendor_grouping": "prod" }
            ],
            "created_at": CLOCK_START_MS,
            "updated_at": CLOCK_START_MS,
            // Authz is off here: the dev principal.
            "updated_by": "anonymous"
        })
    );
    let (status, got) = call(&app, "GET", "/v1/orgs/acme/environments/prod", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, created);

    // Omitted deadline and description take their defaults on a replace —
    // a full replace, not a merge — and created_at stays.
    clock.advance(MINUTE_MS);
    let (status, replaced) = call(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/prod",
        None,
        Some(json!({ "deployment_sources": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(
        replaced["deployment_deadline_ms"],
        json!(DEFAULT_DEPLOYMENT_DEADLINE_MS)
    );
    assert_eq!(replaced["description"], json!(""));
    assert_eq!(replaced["deployment_sources"], json!([]));
    assert_eq!(replaced["created_at"], json!(CLOCK_START_MS));
    assert_eq!(replaced["updated_at"], json!(CLOCK_START_MS + MINUTE_MS));

    call(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/staging",
        None,
        Some(json!({ "deployment_sources": [] })),
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

    for (uri, body) in [
        // Deadline outside 1 minute..7 days.
        (
            "/v1/orgs/acme/environments/prod",
            json!({ "deployment_deadline_ms": MINUTE_MS - 1, "deployment_sources": [] }),
        ),
        (
            "/v1/orgs/acme/environments/prod",
            json!({ "deployment_deadline_ms": 7 * 24 * 60 * MINUTE_MS + 1, "deployment_sources": [] }),
        ),
        // Not a DNS-1123 installation name.
        (
            "/v1/orgs/acme/environments/prod",
            json!({ "deployment_sources": [{ "installation": "Argo CD", "vendor_grouping": "prod" }] }),
        ),
        // Empty vendor grouping.
        (
            "/v1/orgs/acme/environments/prod",
            json!({ "deployment_sources": [{ "installation": "argocd", "vendor_grouping": "" }] }),
        ),
        // The same source twice.
        (
            "/v1/orgs/acme/environments/prod",
            json!({ "deployment_sources": [
                { "installation": "argocd", "vendor_grouping": "prod" },
                { "installation": "argocd", "vendor_grouping": "prod" }
            ] }),
        ),
        // A vendor grouping with an edge space: refused, not kept or trimmed.
        (
            "/v1/orgs/acme/environments/prod",
            json!({ "deployment_sources": [{ "installation": "argocd", "vendor_grouping": " prod" }] }),
        ),
        // A control character in the name.
        (
            "/v1/orgs/acme/environments/prod%09eu",
            json!({ "deployment_sources": [] }),
        ),
        // A dot segment for a name: a client would normalise it away.
        (
            "/v1/orgs/acme/environments/%2E%2E",
            json!({ "deployment_sources": [] }),
        ),
    ] {
        let (status, body_out) = call(&app, "PUT", uri, None, Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {body_out}");
    }
    // A misspelt field is refused rather than silently resetting the
    // deadline; so is a body that does not state the source list.
    for body in [
        json!({ "deadline_ms": 5 * MINUTE_MS, "deployment_sources": [] }),
        json!({ "deployment_deadline_ms": 5 * MINUTE_MS }),
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
        let (status, put) = call(
            &app,
            "PUT",
            &uri,
            None,
            Some(json!({ "deployment_deadline_ms": 5 * MINUTE_MS, "deployment_sources": [] })),
        )
        .await;
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
async fn a_source_another_environment_lists_is_a_409_naming_it() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = open_app(pg, Arc::new(FakeClock::new(CLOCK_START_MS)));

    let source = json!({ "installation": "argocd", "vendor_grouping": "prod" });
    let (status, _) = call(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/prod",
        None,
        Some(json!({ "deployment_sources": [source.clone()] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, message) = call(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/staging",
        None,
        Some(json!({ "deployment_sources": [source.clone()] })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        message.as_str().unwrap().contains("`prod`"),
        "names the holder: {message}"
    );
    let (status, _) = call(
        &app,
        "GET",
        "/v1/orgs/acme/environments/staging",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing was created");
    // Another Org may list the same source.
    let (status, _) = call(
        &app,
        "PUT",
        "/v1/orgs/globex/environments/staging",
        None,
        Some(json!({ "deployment_sources": [source] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    tdb.cleanup().await;
}

/// The Deployment record is keyed by name: deleting an Environment that has
/// Deployments and creating it again would silently re-adopt that history.
#[tokio::test]
async fn an_environment_with_deployments_cannot_be_deleted() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let clock = Arc::new(FakeClock::new(CLOCK_START_MS));
    pg.append("acme", &request("prod", "web", "v1"), clock.now().await.0)
        .await
        .unwrap();
    let app = open_app(pg, clock);
    for env in ["prod", "staging"] {
        call(
            &app,
            "PUT",
            &format!("/v1/orgs/acme/environments/{env}"),
            None,
            Some(json!({ "deployment_sources": [] })),
        )
        .await;
    }

    let (status, _) = call(
        &app,
        "DELETE",
        "/v1/orgs/acme/environments/prod",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(&app, "GET", "/v1/orgs/acme/environments/prod", None, None).await;
    assert_eq!(status, StatusCode::OK, "still there");
    let (status, _) = call(
        &app,
        "DELETE",
        "/v1/orgs/acme/environments/staging",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "no Deployments, so it goes");

    tdb.cleanup().await;
}

/// The deadline the read fold stalls by is the Environment's own; a name with
/// no OrgEnvironment keeps the default.
#[tokio::test]
async fn a_request_stalls_after_its_environments_own_deadline() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let clock = Arc::new(FakeClock::new(CLOCK_START_MS));
    let app = open_app(pg.clone(), clock.clone());
    let (status, _) = call(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/prod",
        None,
        Some(json!({ "deployment_deadline_ms": 5 * MINUTE_MS, "deployment_sources": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Both requested at the same instant: #1 in prod, #2 in staging (which
    // has no OrgEnvironment).
    let at = clock.now().await.0;
    pg.append("acme", &request("prod", "web", "v1"), at)
        .await
        .unwrap();
    pg.append("acme", &request("staging", "web", "s1"), at)
        .await
        .unwrap();
    let status_of = |uri: &'static str| {
        let app = app.clone();
        async move { call(&app, "GET", uri, None, None).await.1["status"].clone() }
    };
    let prod = "/v1/orgs/acme/environments/prod/deployments/1";
    let staging = "/v1/orgs/acme/environments/staging/deployments/2";

    clock.advance(5 * MINUTE_MS - 1);
    assert_eq!(status_of(prod).await, json!("requested"));
    clock.advance(1);
    assert_eq!(status_of(prod).await, json!("stalled"), "prod's 5 minutes");
    assert_eq!(
        status_of(staging).await,
        json!("requested"),
        "staging still has the default hour"
    );
    // The board folds each Environment by its own deadline too.
    let (_, board) = call(&app, "GET", "/v1/orgs/acme/board", None, None).await;
    let statuses: Vec<(String, Value)> = board
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["target"]["environment"].as_str().unwrap().to_string(),
                r["newest"]["status"].clone(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("prod".into(), json!("stalled")),
            ("staging".into(), json!("requested"))
        ]
    );
    clock.advance(DEFAULT_DEPLOYMENT_DEADLINE_MS - 5 * MINUTE_MS);
    assert_eq!(status_of(staging).await, json!("stalled"));

    tdb.cleanup().await;
}

/// Writes need Administer on the Org; reads need Read on the Org. A member
/// of another Org reads nothing; an Org member who is not an admin reads but
/// cannot write.
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
        .with_deployments(pg.clone())
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
    let put_body = || Some(json!({ "deployment_sources": [] }));
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
/// database. (That the fold then falls back to the default deadline is
/// `deadline_from_org_environment`'s contract; this test does not exercise
/// it — the Deployment record is unwired here too.)
#[tokio::test]
async fn unwired_store_is_404() {
    let app = router(app_state(
        Arc::new(InMemoryDb::new()),
        Arc::new(FakeClock::new(0)),
    ));
    for (method, uri, body) in [
        ("GET", "/v1/orgs/acme/environments", None),
        ("GET", "/v1/orgs/acme/environments/prod", None),
        (
            "PUT",
            "/v1/orgs/acme/environments/prod",
            Some(json!({ "deployment_sources": [] })),
        ),
        ("DELETE", "/v1/orgs/acme/environments/prod", None),
    ] {
        let (status, _) = call(&app, method, uri, None, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
}

/// The org Environment store is down while the Deployment record is not.
struct OrgEnvironmentsDown;

#[async_trait::async_trait]
impl scarab_project::OrgEnvironmentStore for OrgEnvironmentsDown {
    async fn put_org_environment(
        &self,
        _: &scarab_project::OrgEnvironment,
    ) -> Result<scarab_project::OrgEnvironment, scarab_project::OrgEnvironmentError> {
        Err(down())
    }
    async fn get_org_environment(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<scarab_project::OrgEnvironment>, scarab_project::OrgEnvironmentError> {
        Err(down())
    }
    async fn list_org_environments(
        &self,
        _: &str,
    ) -> Result<Vec<scarab_project::OrgEnvironment>, scarab_project::OrgEnvironmentError> {
        Err(down())
    }
    async fn delete_org_environment(
        &self,
        _: &str,
        _: &str,
    ) -> Result<bool, scarab_project::OrgEnvironmentError> {
        Err(down())
    }
    async fn environment_for_deployment_source(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<Option<String>, scarab_project::OrgEnvironmentError> {
        Err(down())
    }
}

fn down() -> scarab_project::OrgEnvironmentError {
    scarab_project::OrgEnvironmentError::Store("connection refused".into())
}

/// A Deployment record with one (empty) Environment, `prod`.
struct OneEmptyEnvironment;

#[async_trait::async_trait]
impl DeploymentStore for OneEmptyEnvironment {
    async fn append(
        &self,
        _: &str,
        _: &DeploymentInput,
        _: scarab_deployment::Millis,
    ) -> Result<scarab_deployment::LoggedDeploymentInput, scarab_deployment::DeploymentStoreError>
    {
        Err(scarab_deployment::DeploymentStoreError::Store(
            "read-only fake".into(),
        ))
    }
    async fn inputs(
        &self,
        _: &str,
        _: &str,
    ) -> Result<
        Vec<scarab_deployment::LoggedDeploymentInput>,
        scarab_deployment::DeploymentStoreError,
    > {
        Ok(vec![])
    }
    async fn environments(
        &self,
        _: &str,
    ) -> Result<Vec<String>, scarab_deployment::DeploymentStoreError> {
        Ok(vec!["prod".into()])
    }
}

/// A failed deadline lookup is a 500, never a silent fallback to the default
/// deadline: a status folded against the wrong deadline would read `stalled`
/// (or not) for no reason anyone could see (c-min audit). Needs no database.
#[tokio::test]
async fn a_failed_deadline_lookup_is_a_500_not_the_default_deadline() {
    let app = router(
        app_state(Arc::new(InMemoryDb::new()), Arc::new(FakeClock::new(0)))
            .with_deployments(Arc::new(OneEmptyEnvironment))
            .with_org_environments(Arc::new(OrgEnvironmentsDown)),
    );
    for uri in [
        "/v1/orgs/acme/board",
        "/v1/orgs/acme/board?environment=prod",
        "/v1/orgs/acme/environments/prod/deployments",
    ] {
        let (status, body) = call(&app, "GET", uri, None, None).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{uri}: {body}");
        assert!(
            body.as_str().unwrap().contains("connection refused"),
            "{uri}: {body}"
        );
    }
}
