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
    webhook_request_with_id(event, "delivery-1", body, signature)
}

/// A GitHub delivery with delivery id `delivery` (none when empty).
fn webhook_request_with_id(
    event: &str,
    delivery: &str,
    body: &[u8],
    signature: &str,
) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", event)
        .header("x-hub-signature-256", signature);
    if !delivery.is_empty() {
        request = request.header("x-github-delivery", delivery);
    }
    request.body(Body::from(body.to_vec())).unwrap()
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
    multi_forge_app_with(db, FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML))
}

/// [`multi_forge_app`] over a given forge.
fn multi_forge_app_with(db: Arc<InMemoryDb>, forge: FakeForge) -> axum::Router {
    let clock = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    let forge: Arc<dyn ForgePort> = Arc::new(forge);
    router(
        AppState::new(db.clone(), clock, logs)
            .with_github_webhook_secret(SECRET.to_vec())
            .with_forgejo_webhook_secret(b"forgejo-secret".to_vec())
            .with_forge_connections(db)
            .with_forge(forge),
    )
}

/// Enable `acme/app` (the repo the push bodies name) through a connection of
/// `kind`: with a registry wired, only an enabled repo's events start runs.
async fn enable_app(db: &InMemoryDb, kind: scarab_forge::ForgeKind) {
    use scarab_forge::{ForgeConnection, ForgeConnectionStore, RepoRef};
    db.put_connection(&ForgeConnection {
        id: "acme-conn".into(),
        kind,
        base_url: "https://forge.acme.test".into(),
        credential_ref: "acme-conn-credential".into(),
        org: Some("acme".into()),
        installation_id: None,
        account_id: None,
        account_login: None,
    })
    .await
    .unwrap();
    let app = RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    };
    db.bind_repo("acme-conn", &app, "acme", "app")
        .await
        .unwrap();
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
    enable_app(&db, scarab_forge::ForgeKind::Forgejo).await;
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
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
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

/// A GitHub `installation` webhook registers its ForgeConnection, serving
/// the account's Org, and binds none of its repos: enabling one is opt-in.
/// Before, every covered repo became a Project on install.
#[tokio::test]
async fn an_installation_registers_its_connection_and_binds_nothing() {
    use scarab_forge::ForgeConnectionStore;

    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = multi_forge_app(db.clone());

    let resp = app
        .clone()
        .oneshot(installation_request(
            "install-d1",
            42,
            "acme",
            &["acme/web", "acme/api"],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["registered"]["available"], 2, "{body}");

    let conn = db
        .get_connection("github-install-42")
        .await
        .unwrap()
        .expect("registered connection");
    assert_eq!(conn.kind, scarab_forge::ForgeKind::GitHub);
    assert_eq!(conn.org.as_deref(), Some("acme"));
    assert!(db.repos_of("github-install-42").await.unwrap().is_empty());
    for name in ["web", "api"] {
        assert!(db
            .resolve(&scarab_forge::RepoRef {
                owner: "acme".into(),
                name: name.into(),
            })
            .await
            .unwrap()
            .is_none());
    }
}

/// A signed `installation` delivery for installation `id` on account `login`
/// covering `repos` (`owner/name`).
fn installation_request(delivery: &str, id: u64, login: &str, repos: &[&str]) -> Request<Body> {
    let repositories: Vec<serde_json::Value> = repos
        .iter()
        .map(|r| serde_json::json!({ "full_name": r }))
        .collect();
    let body = serde_json::to_vec(&serde_json::json!({
        "action": "created",
        "installation": { "id": id, "account": { "login": login } },
        "repositories": repositories,
    }))
    .unwrap();
    let sig = scarab_forge_github::sign_hex(SECRET, &body);
    Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "installation")
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", &sig)
        .body(Body::from(body))
        .unwrap()
}

/// git-bug 19b0a45: the installation connection serves the Org of the
/// installation's account and records its installation id (tokens are minted
/// from it alone). It binds none of the delivery's repos, and a repo bound
/// through another Org's connection stays there.
#[tokio::test]
async fn an_installation_serves_its_accounts_org_and_binds_nothing() {
    use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};

    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    // globex/shared is already bound through globex's own connection.
    db.put_connection(&ForgeConnection {
        id: "fj-globex".into(),
        kind: ForgeKind::Forgejo,
        base_url: "https://git.globex.test".into(),
        credential_ref: "fj-globex-token".into(),
        org: Some("globex".into()),
        installation_id: None,
        account_id: None,
        account_login: None,
    })
    .await
    .unwrap();
    let shared = RepoRef {
        owner: "globex".into(),
        name: "shared".into(),
    };
    db.bind_repo("fj-globex", &shared, "globex", "shared")
        .await
        .unwrap();
    let app = multi_forge_app(db.clone());

    let resp = app
        .clone()
        .oneshot(installation_request(
            "inst-org-1",
            7,
            "acme",
            &["acme/web", "globex/shared", "initech/tps"],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let conn = db
        .get_connection("github-install-7")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.org.as_deref(), Some("acme"));
    assert_eq!(conn.installation_id, Some(7));
    assert!(
        db.repos_of("github-install-7").await.unwrap().is_empty(),
        "an installation binds nothing"
    );
    let still = db.resolve(&shared).await.unwrap().unwrap();
    assert_eq!(
        still.connection.id, "fj-globex",
        "globex's repo was not moved"
    );
    assert_eq!(still.org, "globex");
}

/// git-bug 19b0a45 (D7): a GitHub account named like an Org that already has
/// a connection to another forge does not join that Org by name. Its
/// installation is registered unassigned and binds nothing until a global
/// admin acts on it.
#[tokio::test]
async fn an_installation_does_not_join_a_same_named_org_of_another_forge() {
    use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};

    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    db.put_connection(&ForgeConnection {
        id: "fj-acme".into(),
        kind: ForgeKind::Forgejo,
        base_url: "https://git.acme.test".into(),
        credential_ref: "fj-acme-token".into(),
        org: Some("acme".into()),
        installation_id: None,
        account_id: None,
        account_login: None,
    })
    .await
    .unwrap();
    let app = multi_forge_app(db.clone());

    let resp = app
        .clone()
        .oneshot(installation_request("inst-d7-1", 9, "acme", &["acme/web"]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let conn = db
        .get_connection("github-install-9")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.org, None, "registered unassigned");
    assert_eq!(conn.installation_id, Some(9));
    assert!(db.repos_of("github-install-9").await.unwrap().is_empty());
    assert!(db
        .resolve(&RepoRef {
            owner: "acme".into(),
            name: "web".into()
        })
        .await
        .unwrap()
        .is_none());
}

/// An installation delivery with no account login names no Org: refused,
/// and nothing is registered.
#[tokio::test]
async fn an_installation_without_an_account_login_is_refused() {
    use scarab_forge::ForgeConnectionStore;

    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = multi_forge_app(db.clone());
    let resp = app
        .clone()
        .oneshot(installation_request(
            "inst-nologin-1",
            11,
            "",
            &["acme/web"],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(db
        .get_connection("github-install-11")
        .await
        .unwrap()
        .is_none());
}

/// git-bug 19b0a45: an unassigned installation connection that an old
/// re-sync left holding another owner's Project does not take the
/// account's Org on its next delivery. Assigning it would hand globex's
/// Project to acme's admins, who could then unbind it. It stays unassigned
/// and binds nothing new.
#[tokio::test]
async fn a_delivery_does_not_assign_a_connection_holding_another_owners_repo() {
    use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};

    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    db.put_connection(&ForgeConnection {
        id: "github-install-5".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: "github-app".into(),
        org: None,
        installation_id: Some(5),
        account_id: None,
        account_login: None,
    })
    .await
    .unwrap();
    for (owner, name) in [("acme", "web"), ("globex", "site")] {
        let r = RepoRef {
            owner: owner.into(),
            name: name.into(),
        };
        db.bind_repo("github-install-5", &r, owner, name)
            .await
            .unwrap();
    }
    let app = multi_forge_app(db.clone());

    let resp = app
        .clone()
        .oneshot(installation_request("inst-h1-1", 5, "acme", &["acme/new"]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let conn = db
        .get_connection("github-install-5")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.org, None, "left unassigned");
    assert!(db
        .resolve(&RepoRef {
            owner: "acme".into(),
            name: "new".into()
        })
        .await
        .unwrap()
        .is_none());
    let globex = db
        .resolve(&RepoRef {
            owner: "globex".into(),
            name: "site".into(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(globex.org, "globex");
}

// ---------------------------------------------------------------------------
// Opt-in repositories: a GitHub installation makes its repos available, an
// admin enables the ones Scarab builds, and nothing the forge sends enables or
// re-enables one.
// ---------------------------------------------------------------------------

/// A signed `installation_repositories` delivery for installation `id`:
/// `added` and `removed` are `owner/name`.
fn installation_repositories_request(
    delivery: &str,
    id: u64,
    login: &str,
    added: &[&str],
    removed: &[&str],
) -> Request<Body> {
    let full = |repos: &[&str]| -> Vec<serde_json::Value> {
        repos
            .iter()
            .map(|r| serde_json::json!({ "full_name": r }))
            .collect()
    };
    let body = serde_json::to_vec(&serde_json::json!({
        "action": "added",
        "installation": { "id": id, "account": { "login": login } },
        "repositories_added": full(added),
        "repositories_removed": full(removed),
    }))
    .unwrap();
    let sig = scarab_forge_github::sign_hex(SECRET, &body);
    Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "installation_repositories")
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", &sig)
        .body(Body::from(body))
        .unwrap()
}

/// A signed GitHub push to `acme/app` with delivery id `delivery`.
fn push_request(delivery: &str) -> Request<Body> {
    let body = push_body();
    let sig = scarab_forge_github::sign_hex(SECRET, &body);
    Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "push")
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", &sig)
        .body(Body::from(body))
        .unwrap()
}

async fn json_of(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// An unauthenticated request: the app has no session store, so it acts as
/// the dev owner.
fn api(method: &str, uri: &str, body: Option<serde_json::Value>) -> Request<Body> {
    let req = Request::builder().method(method).uri(uri);
    match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap()
}

/// A push to a repo the App covers but nobody enabled is acknowledged with a
/// 2xx (so GitHub does not retry it) and starts nothing, although the repo
/// has a pipeline that matches. Once enabled, the same push starts a run.
#[tokio::test]
async fn a_push_to_a_repo_that_is_not_enabled_is_acknowledged_and_ignored() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = multi_forge_app(db.clone());
    let resp = app
        .clone()
        .oneshot(installation_request("optin-i1", 42, "acme", &["acme/app"]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app.clone().oneshot(push_request("optin-p1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_of(resp).await;
    assert_eq!(body["ignored"], "repository not enabled", "{body}");
    assert!(db.active_runs().await.unwrap().is_empty(), "no run");

    let resp = app
        .clone()
        .oneshot(api(
            "POST",
            "/v1/connections/github-install-42/repos",
            Some(serde_json::json!({ "owner": "acme", "name": "app", "register_webhook": false })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app.clone().oneshot(push_request("optin-p2")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(db.active_runs().await.unwrap().len(), 1);
}

/// Disabling is permanent: a repo an admin disabled is not bound again by a
/// later `installation_repositories` delivery that lists it, nor by a
/// re-sync, which lists it as available.
#[tokio::test]
async fn a_disabled_repo_stays_disabled_after_a_later_delivery() {
    use scarab_forge::ForgeConnectionStore;

    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = multi_forge_app_with(
        db.clone(),
        FakeForge::new()
            .with_file(".scarab/ci.yaml", CI_YAML)
            .with_accessible_repos(&[("acme", "app")]),
    );
    let app_repo = scarab_forge::RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    };
    let resp = app
        .clone()
        .oneshot(installation_request("dis-i1", 42, "acme", &["acme/app"]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(api(
            "POST",
            "/v1/connections/github-install-42/repos",
            Some(serde_json::json!({ "owner": "acme", "name": "app", "register_webhook": false })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(api(
            "DELETE",
            "/v1/connections/github-install-42/repos/acme/app",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // GitHub sends the repo again (removed from the App and added back).
    let resp = app
        .clone()
        .oneshot(installation_repositories_request(
            "dis-r1",
            42,
            "acme",
            &["acme/app"],
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(db.resolve(&app_repo).await.unwrap().is_none());

    let resp = app
        .clone()
        .oneshot(api(
            "POST",
            "/v1/connections/github-install-42/resync",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_of(resp).await;
    assert_eq!(body["available"], serde_json::json!(["acme/app"]), "{body}");
    assert!(db.resolve(&app_repo).await.unwrap().is_none());

    // And its pushes stay ignored.
    let resp = app.clone().oneshot(push_request("dis-p1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(db.active_runs().await.unwrap().is_empty());
}

/// Repos an older server bound on install stay bound: a later delivery that
/// adds other repos, and a re-sync, leave the existing Project where it is
/// and enable nothing new. Nothing migrates bindings away.
#[tokio::test]
async fn existing_bindings_survive_later_deliveries_and_a_resync() {
    use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};

    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    // As an older server left it: the installation's connection, and the
    // repo it bound when the App was installed.
    db.put_connection(&ForgeConnection {
        id: "github-install-42".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: "github-app".into(),
        org: Some("acme".into()),
        installation_id: Some(42),
        account_id: None,
        account_login: None,
    })
    .await
    .unwrap();
    let web = RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    };
    let api_repo = RepoRef {
        owner: "acme".into(),
        name: "api".into(),
    };
    db.bind_repo("github-install-42", &web, "acme", "web")
        .await
        .unwrap();
    let app = multi_forge_app_with(
        db.clone(),
        FakeForge::new().with_accessible_repos(&[("acme", "web"), ("acme", "api")]),
    );

    for (delivery, req) in [
        (
            "keep-i1",
            installation_request("keep-i1", 42, "acme", &["acme/web", "acme/api"]),
        ),
        (
            "keep-r1",
            installation_repositories_request("keep-r1", 42, "acme", &["acme/api"], &[]),
        ),
    ] {
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{delivery}");
        let hit = db.resolve(&web).await.unwrap().expect("still bound");
        assert_eq!(hit.connection.id, "github-install-42", "{delivery}");
        assert_eq!((hit.org.as_str(), hit.project.as_str()), ("acme", "web"));
        assert!(db.resolve(&api_repo).await.unwrap().is_none(), "{delivery}");
    }

    let resp = app
        .clone()
        .oneshot(api(
            "POST",
            "/v1/connections/github-install-42/resync",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_of(resp).await;
    assert_eq!(body["confirmed"], 1, "{body}");
    assert_eq!(body["available"], serde_json::json!(["acme/api"]), "{body}");
    assert!(db.resolve(&web).await.unwrap().is_some());
    assert!(db.resolve(&api_repo).await.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Unreachable repositories: a bound repo the installation stops covering
// keeps its Project and is marked unreachable; nothing the forge sends
// deletes it.
// ---------------------------------------------------------------------------

/// A signed `installation` delivery with `action` for installation `id`;
/// `repos` (`owner/name`) are its `repositories`, omitted when `None`.
fn installation_action_request(
    delivery: &str,
    id: u64,
    login: &str,
    action: &str,
    repos: Option<&[&str]>,
) -> Request<Body> {
    let mut body = serde_json::json!({
        "action": action,
        "installation": { "id": id, "account": { "login": login } },
    });
    if let Some(repos) = repos {
        body["repositories"] = repos
            .iter()
            .map(|r| serde_json::json!({ "full_name": r }))
            .collect();
    }
    let body = serde_json::to_vec(&body).unwrap();
    let sig = scarab_forge_github::sign_hex(SECRET, &body);
    Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "installation")
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", &sig)
        .body(Body::from(body))
        .unwrap()
}

/// The installation 42 connection of acme with `names` bound, as an admin
/// would have enabled them.
async fn seed_installation(db: &InMemoryDb, names: &[&str]) {
    use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
    db.put_connection(&ForgeConnection {
        id: "github-install-42".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: "github-app".into(),
        org: Some("acme".into()),
        installation_id: Some(42),
        account_id: None,
        account_login: None,
    })
    .await
    .unwrap();
    for name in names {
        let r = RepoRef {
            owner: "acme".into(),
            name: (*name).into(),
        };
        db.bind_repo("github-install-42", &r, "acme", name)
            .await
            .unwrap();
    }
}

/// `acme/<name>`'s binding: (still bound?, unreachable since).
async fn reach_of(db: &InMemoryDb, name: &str) -> (bool, Option<i64>) {
    use scarab_forge::ForgeConnectionStore;
    match db
        .resolve(&scarab_forge::RepoRef {
            owner: "acme".into(),
            name: name.into(),
        })
        .await
        .unwrap()
    {
        Some(r) => (true, r.unreachable_since),
        None => (false, None),
    }
}

/// A repo removed from the App's installation keeps its binding and its
/// Project, marked unreachable since the delivery (the app's clock reads
/// 1000). The API shows the mark. Before, the removal unbound it, which
/// deleted the Project with its Environments, secrets and grants.
#[tokio::test]
async fn a_repo_removed_from_the_installation_is_marked_unreachable_and_kept() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    seed_installation(&db, &["web", "api"]).await;
    let app = multi_forge_app(db.clone());

    let resp = app
        .clone()
        .oneshot(installation_repositories_request(
            "unr-r1",
            42,
            "acme",
            &[],
            &["acme/web"],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_of(resp).await;
    assert_eq!(body["registered"]["unreachable"], 1, "{body}");

    assert_eq!(reach_of(&db, "web").await, (true, Some(1_000)));
    assert_eq!(reach_of(&db, "api").await, (true, None));

    let resp = app
        .clone()
        .oneshot(api("GET", "/v1/repos", None))
        .await
        .unwrap();
    let repos = json_of(resp).await;
    let web = repos
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["project"] == "web")
        .expect("the Project is still listed");
    assert_eq!(web["unreachable_since"], 1_000);
}

/// An `installation_repositories` delivery that adds the repo back clears
/// the mark.
#[tokio::test]
async fn a_later_added_delivery_clears_the_unreachable_mark() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    seed_installation(&db, &["web"]).await;
    let app = multi_forge_app(db.clone());

    for (delivery, added, removed) in [
        ("clr-r1", &[][..], &["acme/web"][..]),
        ("clr-r2", &["acme/web"][..], &[][..]),
    ] {
        let resp = app
            .clone()
            .oneshot(installation_repositories_request(
                delivery, 42, "acme", added, removed,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{delivery}");
    }
    assert_eq!(reach_of(&db, "web").await, (true, None));
}

/// A re-sync whose forge lists the repo clears the mark.
#[tokio::test]
async fn a_resync_that_lists_the_repo_clears_the_unreachable_mark() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    seed_installation(&db, &["web"]).await;
    let app = multi_forge_app_with(
        db.clone(),
        FakeForge::new().with_accessible_repos(&[("acme", "web")]),
    );
    let resp = app
        .clone()
        .oneshot(installation_repositories_request(
            "rsy-r1",
            42,
            "acme",
            &[],
            &["acme/web"],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(reach_of(&db, "web").await, (true, Some(1_000)));

    let resp = app
        .clone()
        .oneshot(api(
            "POST",
            "/v1/connections/github-install-42/resync",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json_of(resp).await["confirmed"], 1);
    assert_eq!(reach_of(&db, "web").await, (true, None));
}

/// Uninstalling the App marks every repo bound through its connection
/// unreachable, listed in the delivery or not, and keeps them all;
/// `unsuspend` covering them clears the marks; `suspend` (which lists no
/// repositories) marks them all again.
#[tokio::test]
async fn uninstalling_or_suspending_the_app_marks_all_its_repos_unreachable() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    seed_installation(&db, &["web", "api"]).await;
    let app = multi_forge_app(db.clone());

    let resp = app
        .clone()
        .oneshot(installation_action_request(
            "gone-1",
            42,
            "acme",
            "deleted",
            Some(&["acme/web"]),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(reach_of(&db, "web").await, (true, Some(1_000)));
    assert_eq!(reach_of(&db, "api").await, (true, Some(1_000)));

    let resp = app
        .clone()
        .oneshot(installation_action_request(
            "gone-2",
            42,
            "acme",
            "unsuspend",
            Some(&["acme/web", "acme/api"]),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(reach_of(&db, "web").await, (true, None));
    assert_eq!(reach_of(&db, "api").await, (true, None));

    let resp = app
        .clone()
        .oneshot(installation_action_request(
            "gone-3", 42, "acme", "suspend", None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(reach_of(&db, "web").await, (true, Some(1_000)));
    assert_eq!(reach_of(&db, "api").await, (true, Some(1_000)));
}

/// A push to an unreachable repo is acknowledged with a 2xx (no retry) and
/// starts nothing, although the repo has a matching pipeline.
#[tokio::test]
async fn an_event_for_an_unreachable_repo_is_acknowledged_and_ignored() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    seed_installation(&db, &["app"]).await;
    let app = multi_forge_app(db.clone());
    let resp = app
        .clone()
        .oneshot(installation_repositories_request(
            "ign-r1",
            42,
            "acme",
            &[],
            &["acme/app"],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app.clone().oneshot(push_request("ign-p1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_of(resp).await;
    assert_eq!(body["ignored"], "repository unreachable", "{body}");
    assert!(db.active_runs().await.unwrap().is_empty(), "no run");
}

/// A manual dispatch of an unreachable repo is refused with a 409 that says
/// why, and no run is created.
#[tokio::test]
async fn manual_dispatch_of_an_unreachable_repo_is_refused() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    seed_installation(&db, &["app"]).await;
    let app = multi_forge_app(db.clone());
    let resp = app
        .clone()
        .oneshot(installation_repositories_request(
            "dsp-r1",
            42,
            "acme",
            &[],
            &["acme/app"],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(api(
            "POST",
            "/v1/repos/acme/app/dispatch",
            Some(serde_json::json!({ "ref": "refs/heads/main", "pipeline": "ci" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let msg = String::from_utf8_lossy(&bytes);
    assert!(msg.contains("unreachable"), "{msg}");
    assert!(db.active_runs().await.unwrap().is_empty(), "no run");
}

// ---------------------------------------------------------------------------
// A Pipeline that does not become a Run still posts its check (git-bug
// e5089fd, 2a680c8)
// ---------------------------------------------------------------------------

/// A Pipeline named `checks` that runs on push and does not compile: its one
/// Step needs a Step that is not there.
const BROKEN_YAML: &str = r#"
name: checks
on:
  push: {}
steps:
  - { id: build, image: busybox, command: ["true"], needs: [ghost] }
"#;

/// A Pipeline whose push trigger's `when:` compiles but cannot be decided:
/// the event has no field `nope`.
const UNDECIDABLE_WHEN_YAML: &str = r#"
on:
  push:
    when: 'event.nope == "x"'
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

/// [`multi_forge_app`] over a forge the test keeps, to drain its statuses.
fn multi_forge_app_arc(db: Arc<InMemoryDb>, forge: Arc<FakeForge>) -> axum::Router {
    router(multi_forge_state(db, forge))
}

/// The state [`multi_forge_app_arc`] serves, for a test that also needs it.
fn multi_forge_state(db: Arc<InMemoryDb>, forge: Arc<FakeForge>) -> AppState {
    let clock = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    let forge: Arc<dyn ForgePort> = forge;
    AppState::new(db.clone(), clock, logs)
        .with_github_webhook_secret(SECRET.to_vec())
        .with_forgejo_webhook_secret(b"forgejo-secret".to_vec())
        .with_forge_connections(db)
        .with_forge(forge)
}

/// Post every queued commit status the way the server's tick does, and
/// return every status on the pushed commit (`abc123`) so far.
async fn statuses_on_push(forge: &FakeForge, db: &InMemoryDb) -> Vec<scarab_forge::Status> {
    scarab_server::drain_forge_statuses(forge, db, "test", 100, 0, "https://scarab.example/")
        .await
        .expect("drain");
    forge.statuses_on("abc123")
}

/// The one `failure` the broken `checks` Pipeline posts: under the context
/// its Runs post under, with its first diagnostic, linking to the repository
/// page that lists the refused Run.
fn assert_compile_failure_status(statuses: &[scarab_forge::Status]) {
    assert_eq!(statuses.len(), 1, "{statuses:?}");
    let status = &statuses[0];
    assert_eq!(status.context, "scarab/checks");
    assert_eq!(status.state, scarab_forge::StatusState::Failure);
    assert_eq!(status.target_url, "https://scarab.example/acme/app");
    let description = status.description.as_deref().unwrap_or_default();
    assert!(
        description.contains("needs unknown step `ghost`"),
        "{description}"
    );
    assert!(
        description.chars().count() <= scarab_forge::STATUS_DESCRIPTION_MAX,
        "{description}"
    );
}

/// git-bug e5089fd: a GitHub push whose Pipeline does not compile creates no
/// Run, so no Run posts its check, and a required check waited for good.
/// The refusal posts it, `failure`, once: a replay of the delivery queues
/// nothing new.
#[tokio::test]
async fn a_github_push_whose_pipeline_does_not_compile_fails_its_check_once() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = Arc::new(FakeForge::new().with_file(".scarab/checks.yaml", BROKEN_YAML));
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    let resp = app
        .clone()
        .oneshot(push_request("gh-broken-1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(db.active_runs().await.unwrap().is_empty());
    assert_compile_failure_status(&statuses_on_push(&forge, &db).await);

    let resp = app
        .clone()
        .oneshot(push_request("gh-broken-1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "a replay is acknowledged");
    assert_compile_failure_status(&statuses_on_push(&forge, &db).await);
}

/// The same through the Forgejo endpoint.
#[tokio::test]
async fn a_forgejo_push_whose_pipeline_does_not_compile_fails_its_check_once() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::Forgejo).await;
    let forge = Arc::new(FakeForge::new().with_file(".scarab/checks.yaml", BROKEN_YAML));
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    let body = serde_json::to_vec(&serde_json::json!({
        "ref": "refs/heads/main",
        "after": "abc123",
        "repository": { "name": "app", "owner": { "username": "acme" } }
    }))
    .unwrap();
    let sig = scarab_forge_forgejo::sign_hex(b"forgejo-secret", &body);

    let resp = app
        .clone()
        .oneshot(forgejo_request("push", "fj-broken-1", &body, &sig))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(db.active_runs().await.unwrap().is_empty());
    assert_compile_failure_status(&statuses_on_push(&forge, &db).await);

    let resp = app
        .clone()
        .oneshot(forgejo_request("push", "fj-broken-1", &body, &sig))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "a replay is acknowledged");
    assert_compile_failure_status(&statuses_on_push(&forge, &db).await);
}

/// git-bug 2a680c8: a `when:` that cannot be decided in the first Pipeline
/// file stopped the delivery, so the second file, whose trigger matches,
/// never ran. Now the first is refused on its own (recorded, named in the
/// body, its check failed) and the second starts its Run.
#[tokio::test]
async fn an_undecidable_when_refuses_its_pipeline_and_the_next_file_still_runs() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = Arc::new(
        FakeForge::new()
            .with_file(".scarab/a.yaml", UNDECIDABLE_WHEN_YAML)
            .with_file(".scarab/b.yaml", CI_YAML),
    );
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    let resp = app.oneshot(push_request("gh-when-1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let v = json_of(resp).await;
    assert_eq!(v["run_ids"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(db.active_runs().await.unwrap().len(), 1);
    let refused = v["refused"].as_array().expect("refused listed");
    assert_eq!(refused.len(), 1, "{v}");
    assert_eq!(refused[0]["pipeline"], ".scarab/a.yaml");
    assert!(
        refused[0]["reason"]
            .as_str()
            .unwrap()
            .contains("`when:` of its `push` trigger could not be decided"),
        "{v}"
    );

    let rows = db.refused_runs_for_tenant("acme", "app", 10).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].pipeline, ".scarab/a.yaml");
    assert_eq!(rows[0].reason_kind, "when");

    let failed: Vec<_> = statuses_on_push(&forge, &db)
        .await
        .into_iter()
        .filter(|s| s.state == scarab_forge::StatusState::Failure)
        .collect();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].context, "scarab/a");
    assert_eq!(failed[0].target_url, "https://scarab.example/acme/app");
    assert!(
        failed[0]
            .description
            .as_deref()
            .unwrap_or_default()
            .starts_with(".scarab/a.yaml: "),
        "{failed:?}"
    );
}

/// A delivery with a Pipeline file that does not compile starts no Run, not
/// even for the files before it (ADR-0009), and no check waits on the
/// commit: the broken `checks` fails with its diagnostic, and the CI
/// Pipeline beside it fails as not run, naming the broken file. A replay of
/// the delivery posts nothing new.
#[tokio::test]
async fn a_broken_pipeline_fails_the_check_of_every_pipeline_in_the_delivery_once() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = Arc::new(
        FakeForge::new()
            .with_file(".scarab/a.yaml", CI_YAML)
            .with_file(".scarab/b.yaml", BROKEN_YAML),
    );
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    // The delivery, then its replay.
    for expected in [StatusCode::BAD_REQUEST, StatusCode::OK] {
        let resp = app.clone().oneshot(push_request("gh-two-1")).await.unwrap();
        assert_eq!(resp.status(), expected);
        assert!(db.active_runs().await.unwrap().is_empty(), "no Run at all");

        let statuses = statuses_on_push(&forge, &db).await;
        assert_eq!(statuses.len(), 2, "one each, posted once: {statuses:?}");
        assert!(statuses
            .iter()
            .all(|s| s.state == scarab_forge::StatusState::Failure
                && s.target_url == "https://scarab.example/acme/app"));
        let broken = statuses
            .iter()
            .find(|s| s.context == "scarab/checks")
            .expect("the broken Pipeline's check");
        assert!(
            broken
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("needs unknown step `ghost`"),
            "{broken:?}"
        );
        let other = statuses
            .iter()
            .find(|s| s.context == "scarab/a")
            .expect("the other Pipeline's check");
        assert_eq!(
            other.description.as_deref(),
            Some("not run: .scarab/b.yaml failed to compile")
        );
    }
}

/// A Pipeline kept to `main` by its trigger's `when:`.
const MAIN_ONLY_YAML: &str = r#"
name: release
on:
  push:
    when: "event.branch == 'main'"
steps:
  - { id: ship, image: busybox, command: ["true"] }
"#;

/// A signed GitHub push of `abc123` to `git_ref` with delivery id `delivery`.
fn push_request_to(delivery: &str, git_ref: &str) -> Request<Body> {
    let body = serde_json::to_vec(&serde_json::json!({
        "ref": git_ref,
        "after": "abc123",
        "repository": { "name": "app", "owner": { "login": "acme" } }
    }))
    .unwrap();
    let sig = scarab_forge_github::sign_hex(SECRET, &body);
    Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "push")
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", &sig)
        .body(Body::from(body))
        .unwrap()
}

/// The contexts of the `failure` statuses on the pushed commit, sorted.
async fn failed_contexts(forge: &FakeForge, db: &InMemoryDb) -> Vec<String> {
    let mut contexts: Vec<String> = statuses_on_push(forge, db)
        .await
        .into_iter()
        .filter(|s| s.state == scarab_forge::StatusState::Failure)
        .map(|s| s.context)
        .collect();
    contexts.sort();
    contexts
}

/// A broken file, a Pipeline for every push and one kept to `main`.
fn broken_ci_and_release() -> Arc<FakeForge> {
    Arc::new(
        FakeForge::new()
            .with_file(".scarab/broken.yaml", BROKEN_YAML)
            .with_file(".scarab/ci.yaml", CI_YAML)
            .with_file(".scarab/release.yaml", MAIN_ONLY_YAML),
    )
}

/// REV-m9: a broken file stops the delivery, and only the Pipelines the
/// event would have started fail their checks. A push to a feature branch
/// fails the broken file's check and the CI Pipeline's; the Pipeline kept
/// to `main` by its `when:` would not have run, and gets no red check
/// because another file is broken.
#[tokio::test]
async fn a_broken_file_fails_only_the_pipelines_this_event_would_have_started() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = broken_ci_and_release();
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    let resp = app
        .oneshot(push_request_to("gh-feature-1", "refs/heads/feature"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(db.active_runs().await.unwrap().is_empty(), "no Run at all");
    assert_eq!(
        failed_contexts(&forge, &db).await,
        ["scarab/checks", "scarab/ci"]
    );
    assert!(
        forge
            .statuses_on("abc123")
            .iter()
            .all(|s| s.context != "scarab/release"),
        "the main-only Pipeline posts nothing"
    );
}

/// The same files pushed to `main`: every Pipeline would have run, so every
/// check fails.
#[tokio::test]
async fn a_broken_file_on_main_fails_every_matching_pipeline() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = broken_ci_and_release();
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    let resp = app
        .oneshot(push_request_to("gh-main-1", "refs/heads/main"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        failed_contexts(&forge, &db).await,
        ["scarab/checks", "scarab/ci", "scarab/release"]
    );
    let release = forge
        .statuses_on("abc123")
        .into_iter()
        .find(|s| s.context == "scarab/release")
        .expect("the release check");
    assert_eq!(
        release.description.as_deref(),
        Some("not run: .scarab/broken.yaml failed to compile")
    );
}

/// A Pipeline whose trigger `when:` cannot be decided may have run, so in a
/// delivery a broken file stopped its check fails too (fail closed).
#[tokio::test]
async fn an_undecidable_when_in_a_broken_delivery_still_fails_its_check() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = Arc::new(
        FakeForge::new()
            .with_file(".scarab/a.yaml", UNDECIDABLE_WHEN_YAML)
            .with_file(".scarab/broken.yaml", BROKEN_YAML),
    );
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    let resp = app.oneshot(push_request("gh-undecided-1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        failed_contexts(&forge, &db).await,
        ["scarab/a", "scarab/checks"]
    );
}

/// A deploy Pipeline into `prod`, with the approval gate an Environment
/// deploy carries.
const DEPLOY_YAML: &str = r#"
name: deploy
on:
  push: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], needs: [approve] }
"#;

/// REV-m10: a refusal is posted on every event refused, not once per
/// commit. `prod` does not admit `app`: the first push fails the deploy
/// check. An admin admits `app`, a second delivery on the same commit starts
/// a Run, and the Run's `success` is posted under that context (here
/// straight to the forge, as the status drain posts it when the Run
/// succeeds). The admin takes `app` out again and a third delivery is
/// refused for the same reason: its `failure` is posted again, so the check
/// does not keep the earlier Run's `success`.
#[tokio::test]
async fn a_refusal_after_a_later_success_on_the_same_commit_is_posted_again() {
    use scarab_project::{EnvironmentGovernanceStore, EnvironmentProjectEligibility};
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let checkout_only = EnvironmentProjectEligibility::Projects(vec!["checkout".into()]);
    let governance = Arc::new(
        scarab_testkit::InMemoryEnvironmentGovernance::new().with_environment(
            "acme",
            "prod",
            checkout_only.clone(),
        ),
    );
    let forge = Arc::new(FakeForge::new().with_file(".scarab/deploy.yaml", DEPLOY_YAML));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    let forge_port: Arc<dyn ForgePort> = forge.clone();
    let app = router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
            .with_github_webhook_secret(SECRET.to_vec())
            .with_forge_connections(db.clone())
            .with_environment_governance(governance.clone())
            .with_forge(forge_port),
    );
    // The last status posted under the deploy Pipeline's context.
    let deploy_check = || async {
        statuses_on_push(&forge, &db)
            .await
            .into_iter()
            .filter(|s| s.context == "scarab/deploy")
            .next_back()
            .expect("a deploy status")
    };
    let set = |eligibility: EnvironmentProjectEligibility| {
        let governance = governance.clone();
        async move {
            governance
                .put_environment_eligibility("acme", "prod", &eligibility, "admin", 2_000)
                .await
                .expect("eligibility")
        }
    };

    let resp = app.clone().oneshot(push_request("gh-env-1")).await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let refused = deploy_check().await;
    assert_eq!(refused.state, scarab_forge::StatusState::Failure);

    set(EnvironmentProjectEligibility::AllProjects).await;
    let resp = app.clone().oneshot(push_request("gh-env-2")).await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    assert_eq!(db.active_runs().await.unwrap().len(), 1, "the deploy ran");
    let app_repo = scarab_forge::RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    };
    forge
        .set_status(
            &app_repo,
            &scarab_forge::Commit {
                sha: "abc123".into(),
                message: String::new(),
            },
            scarab_forge::Status {
                context: "scarab/deploy".into(),
                state: scarab_forge::StatusState::Success,
                target_url: "https://scarab.example/acme/app/runs/1".into(),
                description: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        deploy_check().await.state,
        scarab_forge::StatusState::Success
    );

    set(checkout_only).await;
    let resp = app.clone().oneshot(push_request("gh-env-3")).await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let again = deploy_check().await;
    assert_eq!(again.state, scarab_forge::StatusState::Failure);
    assert_eq!(again.description, refused.description);
}

// ---------------------------------------------------------------------------
// pull_request actions: only code-changing ones start Runs
// ---------------------------------------------------------------------------

/// A Pipeline that runs on pull requests only, like a `push: true` image
/// build gated to PRs.
const PR_ONLY_YAML: &str = r#"
on:
  pull_request: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

const PR_HEAD: &str = "feedface";

/// Both forge endpoints over `forge`, which the test keeps to read the
/// statuses Scarab posted.
fn pr_app(db: Arc<InMemoryDb>, forge: Arc<FakeForge>) -> axum::Router {
    let clock = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    router(
        AppState::new(db.clone(), clock, logs)
            .with_github_webhook_secret(SECRET.to_vec())
            .with_forgejo_webhook_secret(b"forgejo-secret".to_vec())
            .with_forge_connections(db)
            .with_forge(forge),
    )
}

fn pr_payload(action: &str, changes: serde_json::Value, merged: bool) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "action": action,
        "number": 7,
        "changes": changes,
        "pull_request": {
            "number": 7,
            "state": if action == "closed" { "closed" } else { "open" },
            "merged": merged,
            "title": "feat: the widget",
            "base": { "ref": "main" },
            "head": { "sha": PR_HEAD, "repo": { "full_name": "acme/app" } }
        },
        "repository": { "name": "app", "owner": { "login": "acme" }, "full_name": "acme/app" },
        "sender": { "login": "octocat" }
    }))
    .unwrap()
}

fn github_pr_request(delivery: &str, body: &[u8]) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "pull_request")
        .header("x-github-delivery", delivery)
        .header(
            "x-hub-signature-256",
            scarab_forge_github::sign_hex(SECRET, body),
        )
        .body(Body::from(body.to_vec()))
        .unwrap()
}

/// Closing or merging a pull request re-ran its `pull_request` Pipelines,
/// and a build Pipeline pushed its images again (reported on 0.6.1). Such a
/// delivery is acknowledged with the reason and starts nothing: no Run, no
/// commit status, no refused Run. New commits, a reopen and a base branch
/// change still start Runs.
#[tokio::test]
async fn a_closed_or_merged_pull_request_starts_no_run_and_posts_no_status() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = Arc::new(FakeForge::new().with_file(".scarab/ci.yaml", PR_ONLY_YAML));
    let app = pr_app(db.clone(), forge.clone());

    let none = serde_json::Value::Null;
    let ignored = [
        ("closed", none.clone(), false),
        ("closed", none.clone(), true),
        ("labeled", none.clone(), false),
        ("ready_for_review", none.clone(), false),
        (
            "edited",
            serde_json::json!({ "title": { "from": "old" } }),
            false,
        ),
    ];
    for (i, (action, changes, merged)) in ignored.iter().enumerate() {
        let body = pr_payload(action, changes.clone(), *merged);
        let resp = app
            .clone()
            .oneshot(github_pr_request(&format!("ign-{i}"), &body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{action}");
        let reply = json_of(resp).await;
        assert!(
            reply["ignored"]
                .as_str()
                .is_some_and(|s| s.contains(action)),
            "{action} (merged {merged}) is ignored with its reason: {reply}"
        );
    }
    assert!(db.active_runs().await.unwrap().is_empty(), "no Run");
    assert!(forge.statuses().is_empty(), "no commit status");
    assert!(
        db.refused_runs_for_tenant("acme", "app", 10)
            .await
            .unwrap()
            .is_empty(),
        "no refused Run"
    );

    let base = serde_json::json!({ "base": { "ref": { "from": "develop" } } });
    let started = [
        ("synchronize", none.clone()),
        ("reopened", none.clone()),
        ("edited", base),
    ];
    for (i, (action, changes)) in started.iter().enumerate() {
        let body = pr_payload(action, changes.clone(), false);
        let resp = app
            .clone()
            .oneshot(github_pr_request(&format!("run-{i}"), &body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED, "{action} starts a Run");
        assert_eq!(db.active_runs().await.unwrap().len(), i + 1, "{action}");
    }
}

/// Forgejo sends new commits as `synchronized` and labels as
/// `label_updated`, under the same `pull_request` event.
#[tokio::test]
async fn forgejo_pull_request_actions_start_runs_only_for_new_code() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::Forgejo).await;
    let forge = Arc::new(FakeForge::new().with_file(".scarab/ci.yaml", PR_ONLY_YAML));
    let app = pr_app(db.clone(), forge.clone());
    let send = |delivery: &str, body: Vec<u8>| {
        let sig = scarab_forge_forgejo::sign_hex(b"forgejo-secret", &body);
        app.clone()
            .oneshot(forgejo_request("pull_request", delivery, &body, &sig))
    };

    let none = serde_json::Value::Null;
    for (i, (action, merged)) in [
        ("closed", true),
        ("closed", false),
        ("label_updated", false),
    ]
    .into_iter()
    .enumerate()
    {
        let resp = send(
            &format!("fj-ign-{i}"),
            pr_payload(action, none.clone(), merged),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{action}");
        let reply = json_of(resp).await;
        assert!(
            reply["ignored"]
                .as_str()
                .is_some_and(|s| s.contains(action)),
            "{action}: {reply}"
        );
    }
    assert!(db.active_runs().await.unwrap().is_empty(), "no Run");
    assert!(forge.statuses().is_empty(), "no commit status");

    let resp = send("fj-sync", pr_payload("synchronized", none, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(db.active_runs().await.unwrap().len(), 1);
}

/// Runs only when something under `infra/` changed.
const INFRA_ONLY_YAML: &str = r#"
on:
  push:
    when: 'event.changed_paths.exists(p, p.startsWith("infra/"))'
steps:
  - { id: plan, image: busybox, command: ["true"] }
"#;

/// The `note` lines in a Run's event log.
async fn run_notes(db: &InMemoryDb, run: &RunId) -> Vec<String> {
    db.events(run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            scarab_engine::EventPayload::Raw(v) => v["note"].as_str().map(str::to_string),
            _ => None,
        })
        .collect()
}

/// git-bug 0354b5a (ADR-0081): GitHub's push payload does not list a file
/// changed only in a merge's conflict resolution (`infra/main.tf` here). The
/// compare answer for the payload's own `before` and `after` does, so the
/// Pipeline filtered on `infra/` starts. Built from the payload's commit
/// lists, the paths would be known and miss `infra/`, and no Run would start.
#[tokio::test]
async fn a_push_whose_merge_resolution_touches_the_filter_starts_the_run() {
    let body = include_bytes!("../../scarab-forge-github/tests/fixtures/push_merge.json");
    let payload: serde_json::Value = serde_json::from_slice(body).unwrap();
    let id = |k: &str| payload[k].as_str().unwrap().to_string();
    let forge = Arc::new(FakeForge::new().with_file(".scarab/infra.yaml", INFRA_ONLY_YAML));
    forge.set_push_paths(
        &id("before"),
        &id("after"),
        scarab_forge::ChangedPaths::from_paths(
            ["conf/app.yaml", "infra/main.tf", "side/feature.txt"].map(String::from),
        ),
    );
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    let sig = scarab_forge_github::sign_hex(SECRET, body);
    let resp = app
        .oneshot(webhook_request("push", body, &sig))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let runs = db.active_runs().await.unwrap();
    assert_eq!(runs.len(), 1);
    assert!(
        run_notes(&db, &runs[0]).await.is_empty(),
        "the filter was applied"
    );
    assert_eq!(forge.push_paths_calls(), 1);
}

/// git-bug 63dbeee (ADR-0081): Forgejo's push payload does not say a push
/// was forced, and its commits are only the new ones. The compare answer
/// says the old head is not an ancestor: the paths are unknown, the
/// Pipeline filtered on `infra/` starts, and its log says it was a forced
/// push. Built from the payload, the paths would be known, miss `infra/`,
/// and no Run would start.
#[tokio::test]
async fn a_forced_forgejo_push_starts_the_run_with_the_forced_note() {
    let before = "6113728f27ae82c7b1a177c8d03f9e96e0adf246";
    let after = "0d1a26e67d8f5eaf1f6ba5c57fc3c7d91ac0fd1c";
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::Forgejo).await;
    let forge = FakeForge::new().with_file(".scarab/infra.yaml", INFRA_ONLY_YAML);
    forge.set_push_paths(
        before,
        after,
        scarab_forge::ChangedPaths::Unknown(scarab_forge::PathsUnknown::Forced),
    );
    let app = multi_forge_app_with(db.clone(), forge);

    let body = serde_json::to_vec(&serde_json::json!({
        "ref": "refs/heads/main",
        "before": before,
        "after": after,
        "total_commits": 1,
        "commits": [{ "id": after, "added": ["README.md"], "removed": [], "modified": [] }],
        "repository": { "name": "app", "owner": { "username": "acme" } }
    }))
    .unwrap();
    let sig = scarab_forge_forgejo::sign_hex(b"forgejo-secret", &body);
    let resp = app
        .oneshot(forgejo_request("push", "fj-forced", &body, &sig))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let runs = db.active_runs().await.unwrap();
    assert_eq!(runs.len(), 1);
    let notes = run_notes(&db, &runs[0]).await;
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].contains(&scarab_forge::PathsUnknown::Forced.reason()),
        "{notes:?}"
    );
}

/// ADR-0081 §7, git-bug 23e3a09: a forge that stops waiting for the answer
/// closes the connection, and the HTTP server drops the handler. The
/// delivery id is already recorded, so a redelivery is answered "duplicate
/// delivery" and could not repair it. Dropping the router's response future
/// while the compare is pending is what the server does on a disconnect;
/// the Run must still be created.
#[tokio::test]
async fn a_delivery_whose_client_went_away_still_creates_its_runs() {
    let body = include_bytes!("../../scarab-forge-github/tests/fixtures/push_merge.json");
    let payload: serde_json::Value = serde_json::from_slice(body).unwrap();
    let id = |k: &str| payload[k].as_str().unwrap().to_string();
    let forge = Arc::new(FakeForge::new().with_file(".scarab/infra.yaml", INFRA_ONLY_YAML));
    forge.set_push_paths(
        &id("before"),
        &id("after"),
        scarab_forge::ChangedPaths::from_paths(["infra/main.tf".to_string()]),
    );
    let gate = forge.hold_push_paths();
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    let sig = scarab_forge_github::sign_hex(SECRET, body);
    let mut answer = Box::pin(app.oneshot(webhook_request("push", body, &sig)));
    tokio::select! {
        _ = &mut answer => panic!("the delivery was answered before the compare"),
        _ = gate.entered() => {}
    }
    drop(answer);
    gate.release();

    let created = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if !db.active_runs().await.unwrap().is_empty() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(created.is_ok(), "the Run was lost with the dropped handler");
    assert_eq!(db.active_runs().await.unwrap().len(), 1);
}

/// Hold the next `record_delivery` call of `db`: the first channel fires
/// when the call got there, and the call goes on when the second is sent.
fn hold_next_record_delivery(
    db: &InMemoryDb,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    db.interleave_before_next(
        "record_delivery",
        Box::new(move || {
            Box::pin(async move {
                let _ = entered_tx.send(());
                let _ = release_rx.await;
            })
        }),
    );
    (entered_rx, release_tx)
}

/// The red team's case (RT-12): the forge closes the connection while the
/// delivery id is being recorded. Recorded in the handler, before the
/// task was spawned, the id could commit with nothing spawned, and the
/// redelivery was a duplicate. Recorded inside the task, the work goes on
/// and creates the Run.
#[tokio::test]
async fn a_delivery_dropped_while_its_id_is_recorded_still_creates_its_runs() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = Arc::new(FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML));
    let state = multi_forge_state(db.clone(), forge);
    let app = router(state.clone());
    let (entered, release) = hold_next_record_delivery(&db);

    let mut answer = Box::pin(app.clone().oneshot(push_request("delivery-1")));
    tokio::select! {
        _ = &mut answer => panic!("the delivery was answered before its id was recorded"),
        _ = entered => {}
    }
    drop(answer);
    release.send(()).unwrap();

    let running =
        scarab_server::drain_webhook_deliveries(&state, std::time::Duration::from_secs(10)).await;
    assert_eq!(running, 0);
    assert_eq!(db.active_runs().await.unwrap().len(), 1, "the Run was lost");
    let again = app.oneshot(push_request("delivery-1")).await.unwrap();
    assert_eq!(json_of(again).await["ignored"], "duplicate delivery");
}

/// RT-13: a delivery whose forge stopped waiting runs in a task no
/// connection waits for. The shutdown drain waits for it, so a rollout does
/// not kill it after its id was recorded.
#[tokio::test]
async fn the_shutdown_drain_waits_for_a_delivery_whose_client_went_away() {
    let body = include_bytes!("../../scarab-forge-github/tests/fixtures/push_merge.json");
    let payload: serde_json::Value = serde_json::from_slice(body).unwrap();
    let id = |k: &str| payload[k].as_str().unwrap().to_string();
    let forge = Arc::new(FakeForge::new().with_file(".scarab/infra.yaml", INFRA_ONLY_YAML));
    forge.set_push_paths(
        &id("before"),
        &id("after"),
        scarab_forge::ChangedPaths::from_paths(["infra/main.tf".to_string()]),
    );
    let gate = forge.hold_push_paths();
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let state = multi_forge_state(db.clone(), forge);
    let app = router(state.clone());

    let sig = scarab_forge_github::sign_hex(SECRET, body);
    let mut answer = Box::pin(app.oneshot(webhook_request("push", body, &sig)));
    tokio::select! {
        _ = &mut answer => panic!("the delivery was answered before the compare"),
        _ = gate.entered() => {}
    }
    drop(answer);

    let drain = tokio::spawn(async move {
        scarab_server::drain_webhook_deliveries(&state, std::time::Duration::from_secs(20)).await
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(
        !drain.is_finished(),
        "the drain ended while a delivery was running"
    );
    gate.release();
    assert_eq!(drain.await.unwrap(), 0);
    assert_eq!(db.active_runs().await.unwrap().len(), 1);
}

/// RT-13 / m7: deliveries run in tasks of their own, so their number is
/// bounded. A delivery that finds every slot taken is answered 503 before
/// its id is recorded, so the forge's redelivery is processed.
#[tokio::test]
async fn a_delivery_that_finds_every_slot_taken_is_answered_503_and_records_nothing() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = Arc::new(FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML));
    let app = router(
        multi_forge_state(db.clone(), forge)
            .with_webhook_delivery_slots(1, std::time::Duration::ZERO),
    );
    let (entered, release) = hold_next_record_delivery(&db);

    let first = tokio::spawn(app.clone().oneshot(push_request("delivery-1")));
    entered.await.unwrap();
    let second = app
        .clone()
        .oneshot(push_request("delivery-2"))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);

    release.send(()).unwrap();
    assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::ACCEPTED);
    let redelivery = app.oneshot(push_request("delivery-2")).await.unwrap();
    assert_eq!(
        redelivery.status(),
        StatusCode::ACCEPTED,
        "the refused delivery's id was recorded"
    );
}

/// m7: with a registry wired, a verified delivery without a delivery id
/// could never be deduplicated; it is refused, on both endpoints.
#[tokio::test]
async fn a_delivery_without_an_id_is_refused() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let app = multi_forge_app(db.clone());

    let body = push_body();
    let sig = scarab_forge_github::sign_hex(SECRET, &body);
    let github = app
        .clone()
        .oneshot(webhook_request_with_id("push", "", &body, &sig))
        .await
        .unwrap();
    assert_eq!(github.status(), StatusCode::BAD_REQUEST);

    let body = serde_json::to_vec(&serde_json::json!({
        "ref": "refs/heads/main",
        "after": "abc123",
        "repository": { "name": "app", "owner": { "username": "acme" } }
    }))
    .unwrap();
    let sig = scarab_forge_forgejo::sign_hex(b"forgejo-secret", &body);
    let forgejo = app
        .oneshot(forgejo_request("push", "", &body, &sig))
        .await
        .unwrap();
    assert_eq!(forgejo.status(), StatusCode::BAD_REQUEST);
    assert!(db.active_runs().await.unwrap().is_empty());
}

/// RT-11: the forge fails to read the Pipeline file on every try of the
/// first delivery. Nothing was created, so the id is forgotten and the
/// answer is 502; the redelivery of the same id creates the Run. Before,
/// the first try answered "no matching pipeline" and the redelivery was a
/// duplicate. Time is paused: the retries' pauses pass at once.
#[tokio::test(start_paused = true)]
async fn a_delivery_whose_pipeline_could_not_be_read_is_repaired_by_its_redelivery() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = Arc::new(FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML));
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    forge.set_read_error(Some("502 bad gateway"));
    let first = app
        .clone()
        .oneshot(push_request("delivery-1"))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::BAD_GATEWAY);
    assert!(db.active_runs().await.unwrap().is_empty());

    forge.set_read_error(None);
    let redelivery = app.oneshot(push_request("delivery-1")).await.unwrap();
    assert_eq!(redelivery.status(), StatusCode::ACCEPTED);
    assert_eq!(db.active_runs().await.unwrap().len(), 1);
}

/// A read of the Pipeline file that fails once is tried again, and the
/// delivery creates its Run.
#[tokio::test(start_paused = true)]
async fn a_pipeline_read_that_fails_once_is_tried_again() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = Arc::new(FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML));
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    forge.fail_next_reads(1);
    let resp = app.oneshot(push_request("delivery-1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(db.active_runs().await.unwrap().len(), 1);
}

/// One Pipeline file could not be read and another started a Run. A
/// redelivery would start that Run again, so the id stays recorded and the
/// answer says the delivery is incomplete.
#[tokio::test(start_paused = true)]
async fn a_delivery_that_started_runs_with_an_unreadable_file_is_incomplete() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    enable_app(&db, scarab_forge::ForgeKind::GitHub).await;
    let forge = Arc::new(
        FakeForge::new()
            .with_file(".scarab/ci.yaml", CI_YAML)
            .with_file(".scarab/lint.yaml", CI_YAML),
    );
    forge.set_read_error_for(".scarab/lint.yaml", Some("502 bad gateway"));
    let app = multi_forge_app_arc(db.clone(), forge.clone());

    let resp = app
        .clone()
        .oneshot(push_request("delivery-1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = json_of(resp).await;
    assert_eq!(body["incomplete"], true, "{body}");
    assert_eq!(body["run_ids"].as_array().unwrap().len(), 1, "{body}");

    let again = app.oneshot(push_request("delivery-1")).await.unwrap();
    assert_eq!(json_of(again).await["ignored"], "duplicate delivery");
    assert_eq!(db.active_runs().await.unwrap().len(), 1);
}
