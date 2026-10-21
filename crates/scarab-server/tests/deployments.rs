//! The Deployment record's read API (ADR-0070, MMP): the Environment board,
//! an Environment's Deployments, and one Deployment — over HTTP, folded from
//! a *real* Postgres input log.
//!
//! The engine `Db` is the in-memory one: the only thing these endpoints ask
//! of it is a requesting run's tenant (for the run link), which a test stamps
//! directly. The input log is seeded through `DeploymentStore::append`, the
//! same path the request writer and report ingest will use, so the stored
//! times are core's receipt times — which is why the clock starts at the wall
//! clock rather than at a small constant. Skips cleanly when
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
    DeploymentInput, DeploymentRequester, DeploymentStatusReport, DeploymentStore,
    DeploymentTarget, DeploymentVersion, SourceAssertedStatus, DEFAULT_DEPLOYMENT_DEADLINE_MS,
};
use scarab_engine::{Db, RunId};
use scarab_identity::{Binding, BindingOrigin, Principal, RbacStore, Role, Scope};
use scarab_project::EnvironmentStore;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, InMemoryDb, InMemoryObjectStore, InMemoryRbac, InMemorySessions,
};

fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn target(environment: &str, id: &str) -> DeploymentTarget {
    DeploymentTarget {
        environment: environment.into(),
        source: "argocd-prod".into(),
        id: id.into(),
    }
}

fn request(environment: &str, id: &str, version: &str, run: &str) -> DeploymentInput {
    DeploymentInput::Request {
        target: target(environment, id),
        version: DeploymentVersion(version.into()),
        requester: DeploymentRequester {
            run: run.into(),
            step: "bump-gitops".into(),
        },
        // The store stamps `at` and clamps `effective_at` to it; a step that
        // started at the epoch is simply "long before".
        effective_at: 0,
        at: 0,
    }
}

fn report(
    environment: &str,
    id: &str,
    version: &str,
    status: SourceAssertedStatus,
    label: &str,
    link: Option<&str>,
) -> DeploymentInput {
    DeploymentInput::Report {
        target: target(environment, id),
        version: DeploymentVersion(version.into()),
        report: DeploymentStatusReport {
            status,
            label: label.into(),
            link: link.map(String::from),
            // The source's clock — kept as `source_at`, never ordered by.
            at: 42,
        },
    }
}

/// Seed acme's log. Seqs (= Deployment ids where an input creates one):
///
/// | seq | env     | target  | input                          | Deployment |
/// |-----|---------|---------|--------------------------------|------------|
/// | 1   | prod    | web     | request v1 (run-1)             | #1 live    |
/// | 2   | prod    | web     | in_progress "sync wave 5"      |            |
/// | 3   | prod    | web     | live                           |            |
/// | 4   | prod    | api     | live a0 (nobody asked)         | #4 Unexplained |
/// | 5   | prod    | new-svc | request n1 (run-3, other org)  | #5 Unconfirmed |
/// | 6   | prod    | web     | request v2 (run-2)             | #6 in flight |
/// | 7   | staging | web     | request s1 (run-4)             | #7 live    |
/// | 8   | staging | web     | live                           |            |
async fn seed(store: &dyn DeploymentStore, db: &InMemoryDb) {
    use SourceAssertedStatus::{InProgress, Live};
    for input in [
        request("prod", "web", "v1", "run-1"),
        report(
            "prod",
            "web",
            "v1",
            InProgress,
            "sync wave 5: migrate",
            Some("https://argo.example/web/ops/1"),
        ),
        report("prod", "web", "v1", Live, "healthy", None),
        report("prod", "api", "a0", Live, "selfHeal revert", None),
        request("prod", "new-svc", "n1", "run-3"),
        request("prod", "web", "v2", "run-2"),
        request("staging", "web", "s1", "run-4"),
        report("staging", "web", "s1", Live, "healthy", None),
    ] {
        store.append("acme", &input).await.unwrap();
    }
    for (run, org, project) in [
        ("run-1", "acme", "web"),
        ("run-2", "acme", "web"),
        // A run stamped into another org: no link from acme's pages.
        ("run-3", "evil", "web"),
        ("run-4", "acme", "web"),
    ] {
        db.set_run_tenant(&RunId(run.into()), org, project)
            .await
            .unwrap();
    }
}

struct Harness {
    app: axum::Router,
    clock: Arc<FakeClock>,
}

async fn harness(pg: Arc<PostgresDb>) -> Harness {
    let db = Arc::new(InMemoryDb::new());
    seed(pg.as_ref(), &db).await;
    // After seeding, so every stored receipt time is at or before "now".
    let clock = Arc::new(FakeClock::new(wall_clock_ms()));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(
        AppState::new(db, clock.clone(), logs)
            .with_deployments(pg.clone())
            .with_environments(pg),
    );
    Harness { app, clock }
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn ids(list: &Value) -> Vec<u64> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_u64().unwrap())
        .collect()
}

#[tokio::test]
async fn the_board_shows_what_is_live_and_what_is_on_its_way() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let h = harness(pg).await;

    let (status, board) = get_json(&h.app, "/v1/orgs/acme/board").await;
    assert_eq!(status, StatusCode::OK);
    // Ordered by Environment, then source, then target id.
    let rows: Vec<(String, String, Option<u64>, Option<u64>)> = board
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["target"]["environment"].as_str().unwrap().into(),
                r["target"]["id"].as_str().unwrap().into(),
                r["live"]["id"].as_u64(),
                r["newest"]["id"].as_u64(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("prod".into(), "api".into(), Some(4), None),
            ("prod".into(), "new-svc".into(), None, Some(5)),
            ("prod".into(), "web".into(), Some(1), Some(6)),
            ("staging".into(), "web".into(), Some(7), None),
        ]
    );
    let api = &board[0]["live"];
    assert_eq!(api["unexplained"], json!(true));
    assert_eq!(api["requesters"], json!([]));
    let web = &board[2];
    assert_eq!(web["live"]["status"], json!("live"));
    assert_eq!(web["live"]["version"], json!("v1"));
    assert_eq!(web["newest"]["status"], json!("requested"));
    assert_eq!(
        web["newest"]["requesters"],
        json!([{
            "run": "run-2",
            "step": "bump-gitops",
            "effective_at": 0,
            "project": "web"
        }])
    );

    // One Environment only.
    let (_, staging) = get_json(&h.app, "/v1/orgs/acme/board?environment=staging").await;
    assert_eq!(staging.as_array().unwrap().len(), 1);
    assert_eq!(staging[0]["live"]["id"], json!(7));

    // An Org with no log has an empty board, not an error.
    let (status, empty) = get_json(&h.app, "/v1/orgs/initech/board").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty, json!([]));

    tdb.cleanup().await;
}

#[tokio::test]
async fn an_environments_deployments_newest_first_with_display_terms() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let h = harness(pg).await;

    let (status, list) = get_json(&h.app, "/v1/orgs/acme/environments/prod/deployments").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ids(&list),
        vec![6, 5, 4, 1],
        "newest first; staging's 7 absent"
    );

    let by_id = |id: u64| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == json!(id))
            .unwrap()
            .clone()
    };
    let unconfirmed = by_id(5);
    assert_eq!(unconfirmed["unconfirmed"], json!(true));
    assert_eq!(unconfirmed["unexplained"], json!(false));
    assert_eq!(
        unconfirmed["requesters"][0]["project"],
        Value::Null,
        "a run stamped into another org gets no link"
    );
    assert_eq!(by_id(4)["unexplained"], json!(true));
    let v1 = by_id(1);
    assert_eq!(v1["status"], json!("live"));
    assert_eq!(v1["was_ever_live"], json!(true));
    assert_eq!(v1["requesters"][0]["project"], json!("web"));

    // Narrowed to one DeploymentTarget.
    let (_, web) = get_json(
        &h.app,
        "/v1/orgs/acme/environments/prod/deployments?target_source=argocd-prod&target_id=web",
    )
    .await;
    assert_eq!(ids(&web), vec![6, 1]);

    // An Environment with no log is empty, not unknown.
    let (status, none) = get_json(&h.app, "/v1/orgs/acme/environments/dev/deployments").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(none, json!([]));

    tdb.cleanup().await;
}

#[tokio::test]
async fn one_deployment_carries_its_stages_in_order() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let h = harness(pg).await;

    let (status, d) = get_json(&h.app, "/v1/orgs/acme/environments/prod/deployments/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(d["target"]["source"], json!("argocd-prod"));
    assert_eq!(d["target"]["id"], json!("web"));
    let reports = d["reports"].as_array().unwrap();
    assert_eq!(
        reports
            .iter()
            .map(|r| (r["status"].clone(), r["label"].clone(), r["link"].clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                json!("in_progress"),
                json!("sync wave 5: migrate"),
                json!("https://argo.example/web/ops/1")
            ),
            (json!("live"), json!("healthy"), Value::Null),
        ]
    );
    // Ordered by core's receipt time; the source's own time rides along.
    assert!(reports[0]["at"].as_i64().unwrap() <= reports[1]["at"].as_i64().unwrap());
    assert_eq!(reports[0]["source_at"], json!(42));

    // Unknown id, and an id that exists only in another Environment.
    for uri in [
        "/v1/orgs/acme/environments/prod/deployments/999",
        "/v1/orgs/acme/environments/prod/deployments/7",
    ] {
        let (status, _) = get_json(&h.app, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }

    tdb.cleanup().await;
}

/// Status is the fold's verdict at the server's `now`: the in-flight request
/// reads `requested` until the EnvironmentDeploymentDeadline elapses, then
/// `stalled` — nothing stored changed.
#[tokio::test]
async fn a_request_stalls_once_the_deadline_has_elapsed() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let h = harness(pg).await;

    let uri = "/v1/orgs/acme/environments/prod/deployments/6";
    assert_eq!(get_json(&h.app, uri).await.1["status"], json!("requested"));
    // A minute of margin over the deadline absorbs any skew between the
    // database's clock (which stamped the input) and this process's.
    h.clock.advance(DEFAULT_DEPLOYMENT_DEADLINE_MS + 60_000);
    assert_eq!(get_json(&h.app, uri).await.1["status"], json!("stalled"));
    // A live one does not stall.
    let (_, live) = get_json(&h.app, "/v1/orgs/acme/environments/prod/deployments/1").await;
    assert_eq!(live["status"], json!("live"));

    tdb.cleanup().await;
}

/// The repo-scoped legacy route keeps serving approval history, untouched by
/// (and not feeding) the Deployment record.
#[tokio::test]
async fn legacy_approval_history_still_serves() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    pg.record_deployment(&scarab_project::Deployment {
        org: "acme".into(),
        project: "web".into(),
        environment: "prod".into(),
        git_ref: "refs/heads/main".into(),
        run: "run-1".into(),
        approved_by: vec!["alice".into()],
        at: 100,
    })
    .await
    .unwrap();
    let h = harness(pg).await;

    let (status, approvals) =
        get_json(&h.app, "/v1/repos/acme/web/environments/prod/deployments").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(approvals.as_array().unwrap().len(), 1);
    assert_eq!(approvals[0]["approved_by"], json!(["alice"]));
    // The approval is not a Deployment.
    let (_, list) = get_json(&h.app, "/v1/orgs/acme/environments/prod/deployments").await;
    assert_eq!(ids(&list), vec![6, 5, 4, 1]);

    tdb.cleanup().await;
}

/// No store wired → the endpoints do not exist here (404), like every other
/// optional surface. Needs no database.
#[tokio::test]
async fn unwired_store_is_404() {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db, Arc::new(FakeClock::new(0)), logs));
    for uri in [
        "/v1/orgs/acme/board",
        "/v1/orgs/acme/environments/prod/deployments",
        "/v1/orgs/acme/environments/prod/deployments/1",
    ] {
        let (status, _) = get_json(&app, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

/// Read is Org-scoped: an Org member reads; someone bound only in another Org
/// does not. A member bound only to a *Project* is refused too — the known gap
/// until EnvironmentProjectEligibility (ADR-0071) decides who may read an
/// Environment.
#[tokio::test]
async fn reading_needs_read_on_the_org() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let db = Arc::new(InMemoryDb::new());
    seed(pg.as_ref(), &db).await;

    let scoped = |subject: &str| Principal {
        subject: subject.into(),
        display_name: None,
        roles: vec![],
    };
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential("amy-code", scoped("amy"))
            .with_credential("eve-code", scoped("eve"))
            .with_credential("pat-code", scoped("pat")),
    );
    let rbac = Arc::new(InMemoryRbac::new());
    for (subject, scope) in [
        ("amy", Scope::Org("acme".into())),
        ("eve", Scope::Org("evil".into())),
        (
            "pat",
            Scope::Project {
                org: "acme".into(),
                name: "web".into(),
            },
        ),
    ] {
        rbac.grant(
            &Binding {
                subject: subject.into(),
                scope,
                role: Role::Viewer,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    }
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(
        AppState::new(db, Arc::new(FakeClock::new(wall_clock_ms())), logs)
            .with_auth(auth, Arc::new(InMemorySessions::new()))
            .with_rbac(rbac)
            .with_deployments(pg),
    );

    let session = |credential: &'static str| {
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(
                    Request::post("/v1/auth/login")
                        .header("content-type", "application/json")
                        .body(Body::from(json!({ "credential": credential }).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()["session"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let status_as = |session: String, uri: &'static str| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::get(uri)
                    .header("authorization", format!("Bearer {session}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };

    let (amy, eve, pat) = (
        session("amy-code").await,
        session("eve-code").await,
        session("pat-code").await,
    );
    for uri in [
        "/v1/orgs/acme/board",
        "/v1/orgs/acme/environments/prod/deployments",
        "/v1/orgs/acme/environments/prod/deployments/1",
    ] {
        assert_eq!(status_as(amy.clone(), uri).await, StatusCode::OK, "{uri}");
        assert_eq!(
            status_as(eve.clone(), uri).await,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
        assert_eq!(
            status_as(pat.clone(), uri).await,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
    let anonymous = app
        .clone()
        .oneshot(
            Request::get("/v1/orgs/acme/board")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    tdb.cleanup().await;
}
