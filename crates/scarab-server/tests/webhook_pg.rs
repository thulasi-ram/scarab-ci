//! Webhook redelivery (ADR-0083) on Postgres, where a `run.create` Policy's
//! allow decision, the Run's first row and the delivery outcome are written
//! in one transaction by the policy store: the path the in-memory cases in
//! `webhook.rs` cannot reach. Skips cleanly when `SCARAB_TEST_DATABASE_URL`
//! is unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::Db;
use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, ForgePort, RepoRef};
use scarab_policy::policy::Policy;
use scarab_policy::store::{NewPolicy, PolicyScope, PolicyStore};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeForge, InMemoryObjectStore};

const SECRET: &[u8] = b"topsecret";

fn on_push(name: &str) -> String {
    format!(
        "name: {name}\non:\n  push: {{}}\nsteps:\n  - {{ id: build, image: busybox, command: [\"true\"] }}\n"
    )
}

fn push_request(delivery: &str) -> Request<Body> {
    let body = serde_json::to_vec(&json!({
        "ref": "refs/heads/main",
        "after": "abc123",
        "repository": { "name": "app", "owner": { "login": "acme" } }
    }))
    .unwrap();
    Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "push")
        .header("x-github-delivery", delivery)
        .header(
            "x-hub-signature-256",
            scarab_forge_github::sign_hex(SECRET, &body),
        )
        .body(Body::from(body))
        .unwrap()
}

async fn send(app: &axum::Router, delivery: &str) -> (StatusCode, serde_json::Value) {
    let resp = app.clone().oneshot(push_request(delivery)).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// SP1: S1's partial delivery with an enforced `run.create` Policy that
/// allows every Run. Three Runs in all, one allow decision for each, three
/// outcomes, and the delivery finished with no lease. Kills: the key not
/// passed through `RunCreationRow` on the Policy path (every in-memory case
/// passes while production creates `a` and `b` again on a redelivery); a
/// `delivery_outcomes` query on the wrong column. Real time: the forge read
/// of `c.yaml` is tried three times, 2.5 s in all, and paused time would
/// also run the database pool's timers.
#[tokio::test]
async fn a_partial_delivery_under_a_run_create_policy_is_repaired_on_postgres() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let pg = Arc::new(PostgresDb::with_pool(pool.clone()));
    pg.migrate().await.unwrap();
    pg.put_connection(&ForgeConnection {
        id: "acme-conn".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://forge.acme.test".into(),
        credential_ref: "acme-conn-credential".into(),
        org: Some("acme".into()),
        installation_id: None,
        account_id: None,
        account_login: None,
    })
    .await
    .unwrap();
    pg.bind_repo(
        "acme-conn",
        &RepoRef {
            owner: "acme".into(),
            name: "app".into(),
        },
        "acme",
        "app",
    )
    .await
    .unwrap();
    pg.create(&NewPolicy {
        id: "allow-all".into(),
        scope: PolicyScope::Org { org: "acme".into() },
        body: Policy::from_json(json!({
            "name": "allow-all",
            "checkpoint": "run.create",
            "mode": "enforce",
            "rules": [{ "when": "false", "effect": "deny", "message": "never" }]
        }))
        .expect("a valid Policy"),
        author: "admin".into(),
        reason: "test".into(),
        at: 1,
    })
    .await
    .unwrap();

    let (a, b, c) = (on_push("alpha"), on_push("beta"), on_push("gamma"));
    let forge = Arc::new(
        FakeForge::new()
            .with_file(".scarab/a.yaml", a)
            .with_file(".scarab/b.yaml", b)
            .with_file(".scarab/c.yaml", c),
    );
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    let forge_port: Arc<dyn ForgePort> = forge.clone();
    let app = router(
        AppState::new(pg.clone(), Arc::new(FakeClock::new(1_000)), logs)
            .with_github_webhook_secret(SECRET.to_vec())
            .with_forge_connections(pg.clone())
            .with_policies(pg.clone())
            .with_forge(forge_port),
    );

    forge.set_read_error_for(".scarab/c.yaml", Some("502 bad gateway"));
    let (status, body) = send(&app, "d1").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["run_ids"].as_array().unwrap().len(), 2, "{body}");
    forge.set_read_error_for(".scarab/c.yaml", None);
    let (status, body) = send(&app, "d1").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["run_ids"].as_array().unwrap().len(), 1, "{body}");

    let runs = pg.list_runs(100).await.unwrap();
    assert_eq!(runs.len(), 3);
    for run in &runs {
        assert_eq!(
            pg.decisions_for_run(&run.run.0).await.unwrap().len(),
            1,
            "{:?}",
            run.run
        );
    }
    let outcomes = pg.delivery_outcomes("acme-conn", "d1").await.unwrap();
    assert_eq!(outcomes.len(), 3, "{outcomes:?}");
    assert!(outcomes
        .iter()
        .all(|o| o.outcome == scarab_engine::DeliveryOutcomeKind::Run));
    let (unfinished, lease): (bool, Option<i64>) = sqlx::query_as(
        "SELECT unfinished, try_until FROM webhook_deliveries WHERE forge = 'github' AND id = 'd1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!unfinished);
    assert_eq!(lease, None);
    tdb.cleanup().await;
}
