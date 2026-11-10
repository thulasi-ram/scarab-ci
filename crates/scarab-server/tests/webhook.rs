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
