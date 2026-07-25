//! Feature acceptance for the editable secret coverage matrix (ADR-0060 part B,
//! ADR-0037 D) against *real* Postgres via the HTTP surface.
//!
//! The matrix is now the **editor** for repo- and environment-scoped values, so
//! the thing worth proving is that reading it and writing through it agree: a
//! cell written at one column's scope reads back `set` there, the columns that
//! did not get one read `inherited`, and — the point of the whole exercise —
//! `resolve` actually returns the environment's own value where it was set and
//! the repo default everywhere else.
//!
//! "Intentionally unset" markers are advisory: they silence an empty cell and
//! never mask a real value.
//!
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::Db;
use scarab_project::{Environment, EnvironmentStore, ProtectionRules, SecretCoverageStore};
use scarab_secrets::{SecretProvider, SecretScope};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeSecrets, InMemoryObjectStore};

const ORG: &str = "acme";
const REPO: &str = "web";

fn env_rules(environment: &str) -> ProtectionRules {
    ProtectionRules {
        approvers: Vec::new(),
        wait_timer: 0,
        allowed_refs: Vec::new(),
        concurrency: 1,
        secret_scope: SecretScope::Environment {
            org: ORG.into(),
            repo: REPO.into(),
            environment: environment.into(),
        },
        oidc_subject: format!("scarab:{ORG}/{REPO}/{environment}"),
        privileged_images: Vec::new(),
        permit_k8s_overlay: false,
        require_reason: false,
    }
}

struct Harness {
    app: axum::Router,
    secrets: Arc<FakeSecrets>,
}

/// The full editor stack: real PG (environments + coverage markers) and a secret
/// provider we can also query directly, to check resolution rather than trusting
/// the read model that produced it.
async fn harness(pg: Arc<PostgresDb>, environments: &[&str]) -> Harness {
    pg.migrate().await.unwrap();
    for name in environments {
        pg.put_environment(
            ORG,
            REPO,
            &Environment {
                name: (*name).into(),
                protection: env_rules(name),
            },
        )
        .await
        .unwrap();
    }
    let secrets = Arc::new(FakeSecrets::new());
    let db: Arc<dyn Db> = pg.clone();
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    let envs: Arc<dyn EnvironmentStore> = pg.clone();
    let coverage: Arc<dyn SecretCoverageStore> = pg.clone();
    let app = router(
        AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
            .with_environments(envs)
            .with_secrets(secrets.clone())
            .with_secret_coverage(coverage),
    );
    Harness { app, secrets }
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn matrix(app: &axum::Router) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/repos/{ORG}/{REPO}/secrets/matrix"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

/// The status map of one key's row.
fn row(m: &serde_json::Value, key: &str) -> serde_json::Value {
    m["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == key)
        .unwrap_or_else(|| panic!("row {key} present in {m}"))["status"]
        .clone()
}

async fn send(app: &axum::Router, req: Request<Body>, expect: StatusCode) {
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), expect);
}

/// Write a value at a column's scope, exactly as the editor does: through the
/// scoped `/v1/secrets` endpoints, with no environment meaning the repo default.
fn write_cell(key: &str, environment: Option<&str>, value: &str) -> Request<Body> {
    let mut body = serde_json::json!({ "org": ORG, "repo": REPO, "name": key, "value": value });
    if let Some(e) = environment {
        body["environment"] = e.into();
    }
    Request::builder()
        .method("POST")
        .uri("/v1/secrets")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn clear_cell(key: &str, environment: Option<&str>) -> Request<Body> {
    let env = environment.map(|e| format!("&environment={e}")).unwrap_or_default();
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/secrets?org={ORG}&repo={REPO}&name={key}{env}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn an_environment_override_wins_while_the_others_inherit_the_repo_default() {
    let Some(tdb) = fresh_db().await else { return };
    let h = harness(Arc::new(PostgresDb::with_pool(tdb.pool.clone())), &["staging", "prod"]).await;

    // The editor's two writes: a repo default, then a prod-only override.
    send(&h.app, write_cell("API_URL", None, "https://repo"), StatusCode::NO_CONTENT).await;
    send(
        &h.app,
        write_cell("API_URL", Some("prod"), "https://prod"),
        StatusCode::NO_CONTENT,
    )
    .await;

    // The read model: set where written, inherited where not.
    let m = matrix(&h.app).await;
    assert_eq!(
        row(&m, "API_URL"),
        serde_json::json!({ "": "set", "prod": "set", "staging": "inherited" })
    );

    // …and resolution agrees, which is the claim that actually matters. The read
    // model could be right while the write went to the wrong scope.
    let resolved = |environment: &str| {
        let secrets = h.secrets.clone();
        let scope = SecretScope::Environment {
            org: ORG.into(),
            repo: REPO.into(),
            environment: environment.into(),
        };
        async move {
            String::from_utf8(secrets.resolve(&scope, "API_URL").await.unwrap().value).unwrap()
        }
    };
    assert_eq!(resolved("prod").await, "https://prod", "override wins");
    assert_eq!(
        resolved("staging").await,
        "https://repo",
        "an unset environment falls through to the repo default"
    );

    // Clearing the override at that scope alone hands the environment back to
    // the default — the repo value is untouched.
    send(&h.app, clear_cell("API_URL", Some("prod")), StatusCode::NO_CONTENT).await;
    assert_eq!(
        row(&matrix(&h.app).await, "API_URL"),
        serde_json::json!({ "": "set", "prod": "inherited", "staging": "inherited" })
    );
    assert_eq!(resolved("prod").await, "https://repo");

    tdb.cleanup().await;
}

#[tokio::test]
async fn a_cell_names_the_scope_it_would_override() {
    let Some(tdb) = fresh_db().await else { return };
    let h = harness(Arc::new(PostgresDb::with_pool(tdb.pool.clone())), &["prod"]).await;

    // ORG_WIDE lives above the repo; REPO_ONLY at the repo.
    h.secrets
        .put(
            &SecretScope::Org { org: ORG.into() },
            scarab_secrets::Secret { key: "ORG_WIDE".into(), value: b"o".to_vec() },
        )
        .await
        .unwrap();
    send(&h.app, write_cell("REPO_ONLY", None, "r"), StatusCode::NO_CONTENT).await;

    let m = matrix(&h.app).await;
    let from = |key: &str| {
        m["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == key)
            .unwrap()["inherited_from"]
            .clone()
    };
    // The org value falls through to BOTH the repo column and the environment;
    // the repo value only to the environment. Without the origin, an edit can't
    // say what it is overriding.
    assert_eq!(from("ORG_WIDE"), serde_json::json!({ "": "org", "prod": "org" }));
    assert_eq!(from("REPO_ONLY"), serde_json::json!({ "prod": "repo" }));

    tdb.cleanup().await;
}

#[tokio::test]
async fn silencing_annotates_an_empty_cell_and_never_masks_a_value() {
    let Some(tdb) = fresh_db().await else { return };
    let h = harness(Arc::new(PostgresDb::with_pool(tdb.pool.clone())), &["staging", "prod"]).await;

    let silence = |key: &str, environment: Option<&str>| {
        let mut body = serde_json::json!({ "key": key });
        if let Some(e) = environment {
            body["environment"] = e.into();
        }
        Request::builder()
            .method("PUT")
            .uri(format!("/v1/repos/{ORG}/{REPO}/secrets/matrix/silenced"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // A key that exists nowhere: silencing it still surfaces the row, so the
    // annotation is visible (and removable) rather than orphaned.
    send(&h.app, silence("STAGING_ONLY_TOKEN", Some("prod")), StatusCode::NO_CONTENT).await;
    assert_eq!(
        row(&matrix(&h.app).await, "STAGING_ONLY_TOKEN"),
        serde_json::json!({ "": "unset", "prod": "silenced", "staging": "unset" })
    );

    // Idempotent.
    send(&h.app, silence("STAGING_ONLY_TOKEN", Some("prod")), StatusCode::NO_CONTENT).await;

    // Now give that cell a real value: the marker must NOT hide it — a silenced
    // cell that actually resolves would be a lie about coverage.
    send(
        &h.app,
        write_cell("STAGING_ONLY_TOKEN", Some("prod"), "t"),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        row(&matrix(&h.app).await, "STAGING_ONLY_TOKEN")["prod"],
        "set"
    );

    // Clearing the value returns the cell to its marker, which survived.
    send(
        &h.app,
        clear_cell("STAGING_ONLY_TOKEN", Some("prod")),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        row(&matrix(&h.app).await, "STAGING_ONLY_TOKEN")["prod"],
        "silenced"
    );

    // Unsilencing is idempotent and puts the cell back to a plain gap.
    for _ in 0..2 {
        send(
            &h.app,
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/v1/repos/{ORG}/{REPO}/secrets/matrix/silenced?key=STAGING_ONLY_TOKEN&environment=prod"
                ))
                .body(Body::empty())
                .unwrap(),
            StatusCode::NO_CONTENT,
        )
        .await;
    }
    let m = matrix(&h.app).await;
    assert!(
        m["keys"].as_array().unwrap().iter().all(|r| r["key"] != "STAGING_ONLY_TOKEN"),
        "with no value and no marker the row has nothing to say: {m}"
    );

    tdb.cleanup().await;
}

#[tokio::test]
async fn the_repo_default_column_can_be_silenced_too() {
    let Some(tdb) = fresh_db().await else { return };
    let h = harness(Arc::new(PostgresDb::with_pool(tdb.pool.clone())), &["prod"]).await;

    // No `environment` in the body addresses the repo-default column.
    send(
        &h.app,
        Request::builder()
            .method("PUT")
            .uri(format!("/v1/repos/{ORG}/{REPO}/secrets/matrix/silenced"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::json!({ "key": "LEGACY_KEY" }).to_string()))
            .unwrap(),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        row(&matrix(&h.app).await, "LEGACY_KEY"),
        serde_json::json!({ "": "silenced", "prod": "unset" })
    );

    tdb.cleanup().await;
}
