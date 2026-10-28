//! The deploy marker (ADR-0074 §2) against *real* Postgres: a pipeline or a
//! Step marks itself `deploy: true`, the Run is classified once at creation
//! from the IR, and
//!
//!  1. `GET /v1/repos/{org}/{repo}/runs?deploy=` filters on it in the query,
//!     alone or combined with `environment` — which is a separate fact (an
//!     Environment-governed run may be a smoke test);
//!  2. the run summary carries `deploy`, and the Run page marks the deploy Step;
//!  3. a deploy Run is exempt from newest-wins cancel, like an Environment run:
//!     no supersede key, and a `cancel-in-progress` group queues instead.
//!
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{ConcurrencyPolicy, Db, RunId};
use scarab_forge::{Event, RepoRef};
use scarab_server::{router, trigger_run_from_event, AppState, LogService};
use scarab_testkit::{FakeClock, FakeForge, InMemoryObjectStore};

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        repo: RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        r#ref: "refs/heads/main".into(),
        after: "sha123".into(),
        message: "ship it".into(),
    }
}

fn app(pg: Arc<PostgresDb>) -> axum::Router {
    let db: Arc<dyn Db> = pg.clone();
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), pg));
    router(AppState::new(db, Arc::new(FakeClock::new(1_000)), logs))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// The pipeline names of the runs a runs-list query returns.
async fn pipelines(app: &axum::Router, query: &str) -> BTreeSet<String> {
    let (status, body) = get(app, &format!("/v1/repos/acme/web/runs{query}")).await;
    assert_eq!(status, StatusCode::OK, "{query}");
    body["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["pipeline"].as_str().unwrap().to_string())
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn runs_list_filters_by_the_deploy_marker() {
    let Some(tdb) = fresh_db().await else {
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();

    let forge = FakeForge::new()
        // Marked at the pipeline.
        .with_file(
            ".scarab/ship.yaml",
            "on: { push: {} }\ndeploy: true\nsteps: [{ id: helm, image: helm }]",
        )
        // Marked at one Step.
        .with_file(
            ".scarab/release.yaml",
            "on: { push: {} }\nsteps:\n  - { id: build, image: rust }\n  \
             - { id: push, image: crane, needs: [build], deploy: true }",
        )
        // Unmarked.
        .with_file(
            ".scarab/ci.yaml",
            "on: { push: {} }\nsteps: [{ id: test, image: rust }]",
        )
        // Environment-governed, marked.
        .with_file(
            ".scarab/prod-deploy.yaml",
            "on: { push: {} }\nenvironment: production\n\
             steps: [{ id: rollout, image: helm, deploy: true }]",
        )
        // Environment-governed, unmarked: a smoke test is not a deploy.
        .with_file(
            ".scarab/prod-smoke.yaml",
            "on: { push: {} }\nenvironment: production\nsteps: [{ id: curl, image: curl }]",
        );
    let clock = FakeClock::new(1_000);
    let runs = trigger_run_from_event(
        &forge,
        pg.as_ref(),
        &clock,
        scarab_server::AdmissionPorts::governance(None),
        &push(),
    )
    .await
    .expect("trigger");
    assert_eq!(runs.len(), 5, "every pipeline matched");
    let app = app(pg);

    let deploys = set(&["ship", "release", "prod-deploy"]);
    assert_eq!(pipelines(&app, "?deploy=true").await, deploys);
    assert_eq!(
        pipelines(&app, "?deploy=false").await,
        set(&["ci", "prod-smoke"])
    );
    assert_eq!(
        pipelines(&app, "?environment=production&deploy=true").await,
        set(&["prod-deploy"]),
        "combined with environment"
    );
    assert_eq!(
        pipelines(&app, "?environment=production&deploy=false").await,
        set(&["prod-smoke"])
    );
    assert_eq!(pipelines(&app, "").await.len(), 5, "no filter: every run");

    // The summary carries the marker.
    let (_, body) = get(&app, "/v1/repos/acme/web/runs").await;
    let rows = body["runs"].as_array().unwrap();
    for r in rows {
        let name = r["pipeline"].as_str().unwrap();
        assert_eq!(r["deploy"], deploys.contains(name), "{name}");
    }

    // The Run page marks the deploy Step — and only it.
    let release = rows.iter().find(|r| r["pipeline"] == "release").unwrap();
    let (status, run) = get(
        &app,
        &format!("/v1/runs/{}", release["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let marked: Vec<&str> = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["deploy"] == true)
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(marked, ["push"]);
    // A pipeline-level mark names no Step (an unmarked Step omits the field).
    let ship = rows.iter().find(|r| r["pipeline"] == "ship").unwrap();
    let (_, run) = get(&app, &format!("/v1/runs/{}", ship["id"].as_str().unwrap())).await;
    let steps = run["steps"].as_array().unwrap();
    assert!(!steps.is_empty());
    assert!(steps.iter().all(|s| s.get("deploy").is_none()));

    let (status, _) = get(&app, "/v1/repos/acme/web/runs?deploy=maybe").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    tdb.cleanup().await;
}

/// An ungoverned, pipeline-unmarked run with one `deploy: true` Step is a
/// deploy Run, so newest-wins cancel never takes it: a newer run on the same
/// ref does not supersede it, and its `cancel-in-progress` group queues.
#[tokio::test]
async fn a_deploy_run_is_exempt_from_newest_wins_cancel() {
    let Some(tdb) = fresh_db().await else {
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();

    let forge = FakeForge::new().with_file(
        ".scarab/release.yaml",
        "on: { push: {} }\nconcurrency: { group: release, policy: cancel-in-progress }\n\
         steps:\n  - { id: build, image: rust }\n  \
         - { id: push, image: crane, needs: [build], deploy: true }",
    );
    let clock = FakeClock::new(1_000);
    let trigger = || async {
        trigger_run_from_event(
            &forge,
            pg.as_ref(),
            &clock,
            scarab_server::AdmissionPorts::governance(None),
            &push(),
        )
        .await
        .expect("trigger")
        .pop()
        .expect("one run")
    };
    let first: RunId = trigger().await;
    clock.advance(1_000);
    let second: RunId = trigger().await;

    assert!(
        pg.superseded_by(&second).await.unwrap().is_empty(),
        "a newer run must not supersede an in-flight deploy Run"
    );
    let (group, policy) = pg
        .run_concurrency(&first)
        .await
        .unwrap()
        .expect("the run is in its concurrency group");
    assert_eq!(group, "acme/web:release");
    assert_eq!(
        policy,
        ConcurrencyPolicy::Queue,
        "a deploy Run queues regardless of the declared cancel-in-progress"
    );

    // Contrast: the same pipeline without the marker keeps newest-wins.
    let ci = FakeForge::new().with_file(
        ".scarab/release.yaml",
        "on: { push: {} }\nconcurrency: { group: ci, policy: cancel-in-progress }\n\
         steps:\n  - { id: build, image: rust }\n  - { id: push, image: crane, needs: [build] }",
    );
    clock.advance(1_000);
    let older = trigger_run_from_event(
        &ci,
        pg.as_ref(),
        &clock,
        scarab_server::AdmissionPorts::governance(None),
        &push(),
    )
    .await
    .unwrap()
    .pop()
    .unwrap();
    clock.advance(1_000);
    let newer = trigger_run_from_event(
        &ci,
        pg.as_ref(),
        &clock,
        scarab_server::AdmissionPorts::governance(None),
        &push(),
    )
    .await
    .unwrap()
    .pop()
    .unwrap();
    assert_eq!(pg.superseded_by(&newer).await.unwrap(), vec![older.clone()]);
    assert_eq!(
        pg.run_concurrency(&older).await.unwrap().unwrap().1,
        ConcurrencyPolicy::CancelInProgress
    );

    tdb.cleanup().await;
}
