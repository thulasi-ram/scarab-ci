//! Webhook ingest acceptance (ADR-0010, 0032): a signed GitHub push is verified,
//! normalized to a canonical Event, and creates a Run; a bad signature is
//! rejected; an administrative event is acknowledged-and-ignored. Hermetic —
//! the HTTP boundary is driven in-process (InMemoryDb, no network).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{Db, RunId};
use scarab_forge::ForgePort;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb, InMemoryObjectStore};

const SECRET: &[u8] = b"topsecret";

/// A `.scarab/ci.yaml` that runs one step on any push.
const CI_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

fn app(db: Arc<InMemoryDb>) -> axum::Router {
    let clock = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    let forge: Arc<dyn ForgePort> =
        Arc::new(FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML));
    router(
        AppState::new(db, clock, logs)
            .with_github_webhook_secret(SECRET.to_vec())
            .with_forge(forge),
    )
}

fn push_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "ref": "refs/heads/main",
        "after": "abc123",
        "repository": { "name": "app", "owner": { "login": "acme" } }
    }))
    .unwrap()
}

fn webhook_request(event: &str, body: &[u8], signature: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", event)
        .header("x-github-delivery", "delivery-1")
        .header("x-hub-signature-256", signature)
        .body(Body::from(body.to_vec()))
        .unwrap()
}

#[tokio::test]
async fn signed_push_creates_a_run() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = app(db.clone());

    let body = push_body();
    let sig = scarab_forge_github::sign_hex(SECRET, &body);
    let resp = app
        .oneshot(webhook_request("push", &body, &sig))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let run_id = v["run_ids"][0].as_str().unwrap().to_string();
    assert_eq!(v["run_ids"].as_array().unwrap().len(), 1);
    assert_eq!(v["trigger"], "push");

    // The run is durable, and its normalized trigger is on the event log.
    assert!(db
        .run_status(&RunId(run_id.clone()))
        .await
        .unwrap()
        .is_some());
    let events = db.events(&RunId(run_id)).await.unwrap();
    let trigger = events.iter().find_map(|e| match &e.kind {
        scarab_engine::EventPayload::Raw(v) => v.get("trigger").cloned(),
        _ => None,
    });
    assert!(trigger.is_some(), "trigger event persisted on the log");
}

#[tokio::test]
async fn bad_signature_is_rejected_and_creates_no_run() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = app(db.clone());

    let body = push_body();
    let resp = app
        .oneshot(webhook_request("push", &body, "sha256=deadbeef"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        db.active_runs().await.unwrap().is_empty(),
        "no run on bad signature"
    );
}

#[tokio::test]
async fn unsupported_event_is_acknowledged_and_ignored() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = app(db.clone());

    let body = serde_json::to_vec(&serde_json::json!({ "zen": "keep it simple" })).unwrap();
    let sig = scarab_forge_github::sign_hex(SECRET, &body);
    let resp = app
        .oneshot(webhook_request("ping", &body, &sig))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        db.active_runs().await.unwrap().is_empty(),
        "ping creates no run"
    );
}

// ---------------------------------------------------------------------------
// Multi-forge routing + replay dedup (ADR-0046)
// ---------------------------------------------------------------------------

/// App with BOTH forge endpoints + the registry (dedup) wired.
fn multi_forge_app(db: Arc<InMemoryDb>) -> axum::Router {
    let clock = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    let forge: Arc<dyn ForgePort> =
        Arc::new(FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML));
    router(
        AppState::new(db.clone(), clock, logs)
            .with_github_webhook_secret(SECRET.to_vec())
            .with_forgejo_webhook_secret(b"forgejo-secret".to_vec())
            .with_forge_connections(db)
            .with_forge(forge),
    )
}

fn forgejo_request(event: &str, delivery: &str, body: &[u8], signature: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/webhooks/forgejo")
        .header("content-type", "application/json")
        .header("x-forgejo-event", event)
        .header("x-forgejo-delivery", delivery)
        .header("x-forgejo-signature", signature)
        .body(Body::from(body.to_vec()))
        .unwrap()
}

/// A signed Forgejo push routes through ITS endpoint/adapter and creates a run
/// — and each endpoint verifies with its own secret (the GitHub secret does
/// not open the Forgejo door).
#[tokio::test]
async fn forgejo_endpoint_verifies_with_its_own_secret_and_creates_a_run() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = multi_forge_app(db.clone());

    // Forgejo payload variant: owner.username instead of owner.login.
    let body = serde_json::to_vec(&serde_json::json!({
        "ref": "refs/heads/main",
        "after": "abc123",
        "repository": { "name": "app", "owner": { "username": "acme" } }
    }))
    .unwrap();

    // Signed with the WRONG (GitHub) secret => rejected.
    let bad = scarab_forge_forgejo::sign_hex(SECRET, &body);
    let resp = app
        .clone()
        .oneshot(forgejo_request("push", "fj-1", &body, &bad))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Signed with the Forgejo secret => a run starts.
    let sig = scarab_forge_forgejo::sign_hex(b"forgejo-secret", &body);
    let resp = app
        .clone()
        .oneshot(forgejo_request("push", "fj-1", &body, &sig))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(db.active_runs().await.unwrap().len(), 1);
}

/// A replayed (still correctly-signed) delivery is acknowledged WITHOUT
/// re-processing — the delivery-id guard (ADR-0046). Distinct ids process.
#[tokio::test]
async fn replayed_delivery_is_ignored_idempotently() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = multi_forge_app(db.clone());

    let body = push_body();
    let sig = scarab_forge_github::sign_hex(SECRET, &body);
    let req = |delivery: &str| {
        Request::builder()
            .method("POST")
            .uri("/webhooks/github")
            .header("content-type", "application/json")
            .header("x-github-event", "push")
            .header("x-github-delivery", delivery)
            .header("x-hub-signature-256", &sig)
            .body(Body::from(body.clone()))
            .unwrap()
    };

    let resp = app.clone().oneshot(req("gh-d1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(db.active_runs().await.unwrap().len(), 1);

    // The exact same delivery again: acknowledged, NOT re-processed.
    let resp = app.clone().oneshot(req("gh-d1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(db.active_runs().await.unwrap().len(), 1, "no duplicate run");

    // A different delivery id processes normally.
    let resp = app.clone().oneshot(req("gh-d2")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(db.active_runs().await.unwrap().len(), 2);
}

/// A GitHub `installation` webhook auto-registers a ForgeConnection and binds
/// its repos to Projects (ADR-0046: installing the App IS registration).
#[tokio::test]
async fn installation_webhook_auto_registers_the_connection_and_repos() {
    use scarab_forge::ForgeConnectionStore;

    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = multi_forge_app(db.clone());

    let body = serde_json::to_vec(&serde_json::json!({
        "action": "created",
        "installation": { "id": 42, "account": { "login": "acme" } },
        "repositories": [ { "full_name": "acme/web" }, { "full_name": "acme/api" } ]
    }))
    .unwrap();
    let sig = scarab_forge_github::sign_hex(SECRET, &body);
    let req = Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "installation")
        .header("x-github-delivery", "install-d1")
        .header("x-hub-signature-256", &sig)
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The connection exists and both repos resolve to their Projects.
    let conn = db
        .get_connection("github-install-42")
        .await
        .unwrap()
        .expect("auto-registered connection");
    assert_eq!(conn.kind, scarab_forge::ForgeKind::GitHub);
    let hit = db
        .resolve(&scarab_forge::RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        })
        .await
        .unwrap()
        .expect("repo bound");
    assert_eq!((hit.org.as_str(), hit.project.as_str()), ("acme", "web"));
}
