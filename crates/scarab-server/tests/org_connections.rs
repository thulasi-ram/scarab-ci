//! Org-scoped forge connections (git-bug 19b0a45): a connection serves one
//! Org, and an admin of another Org neither sees it nor acts through it.
//!
//! Before this change every `/v1/connections*` route asked only "does the
//! caller administer *some* Org?", so an admin of `globex` could list
//! `acme`'s connections, delete them (with acme's Projects), bind repos
//! through them and re-sync them. Each test below drives one of those
//! paths as globex (a session, and an Org-scoped API token) against acme's
//! connection and checks both the answer and that nothing changed.
//!
//! Hermetic: `InMemoryDb` is the registry (it mirrors the Postgres re-home
//! guard), `FakeForge` the forge, `FakeSecrets` the credential store.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_identity::{
    api_token_hash, ApiToken, ApiTokenStore, Binding, BindingOrigin, Principal, RbacStore, Role,
    Scope,
};
use scarab_secrets::{Secret, SecretProvider, SecretScope};
use scarab_server::{router, AppState, LogService, FORGE_CREDENTIALS_ORG};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeForge, FakeSecrets, InMemoryApiTokens, InMemoryDb,
    InMemoryObjectStore, InMemoryRbac, InMemorySessions,
};

const T0: i64 = 1_000_000;
const DAY: i64 = 24 * 60 * 60 * 1000;
const GLOBEX_TOKEN: &str = "scarab_pat_globex_0123456789abcdef0123456789abcdef";
const WEBHOOK_SECRET: &[u8] = b"topsecret";

struct Harness {
    app: axum::Router,
    db: Arc<InMemoryDb>,
}

fn repo(owner: &str, name: &str) -> RepoRef {
    RepoRef {
        owner: owner.into(),
        name: name.into(),
    }
}

fn conn(
    id: &str,
    kind: ForgeKind,
    base: &str,
    org: Option<&str>,
    inst: Option<u64>,
) -> ForgeConnection {
    ForgeConnection {
        id: id.into(),
        kind,
        base_url: base.into(),
        credential_ref: format!("{id}-credential"),
        org: org.map(str::to_string),
        installation_id: inst,
        account_id: None,
        account_login: None,
    }
}

/// Two Orgs, each with a connection and a bound repo, plus an unassigned
/// GitHub installation. `forge` is what every connection's credential
/// reaches.
async fn harness(forge: FakeForge) -> Harness {
    let db = Arc::new(InMemoryDb::new());
    let secrets = Arc::new(FakeSecrets::new());
    for (c, bound) in [
        (
            conn(
                "gh-acme",
                ForgeKind::GitHub,
                "https://api.github.com",
                Some("acme"),
                Some(1),
            ),
            Some(repo("acme", "web")),
        ),
        (
            conn(
                "fj-globex",
                ForgeKind::Forgejo,
                "https://git.globex.test",
                Some("globex"),
                None,
            ),
            Some(repo("globex", "site")),
        ),
        (
            conn(
                "gh-legacy",
                ForgeKind::GitHub,
                "https://api.github.com",
                None,
                Some(3),
            ),
            None,
        ),
    ] {
        db.put_connection(&c).await.unwrap();
        secrets
            .put(
                &SecretScope::Org {
                    org: FORGE_CREDENTIALS_ORG.into(),
                },
                Secret {
                    key: c.credential_ref.clone(),
                    value: b"material".to_vec(),
                },
            )
            .await
            .unwrap();
        if let Some(r) = bound {
            db.bind_repo(&c.id, &r, &r.owner, &r.name).await.unwrap();
        }
    }

    let rbac = Arc::new(InMemoryRbac::new());
    for (subject, org) in [("acme-admin", "acme"), ("globex-admin", "globex")] {
        rbac.grant(
            &Binding {
                subject: subject.into(),
                scope: Scope::Org(org.into()),
                role: Role::Admin,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    }
    rbac.grant(
        &Binding {
            subject: "acme-reader".into(),
            scope: Scope::Org("acme".into()),
            role: Role::Viewer,
        },
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    let tokens = Arc::new(InMemoryApiTokens::new());
    tokens
        .put(
            &ApiToken {
                id: "tok-globex".into(),
                name: "globex ci".into(),
                owner_subject: "globex-admin".into(),
                scope: Scope::Org("globex".into()),
                role: Role::Admin,
                expires_at: T0 + 30 * DAY,
                created_by: "globex-admin".into(),
                created_at: T0,
                last_used_at: None,
                use_count: 0,
                revoked_at: None,
            },
            &api_token_hash(GLOBEX_TOKEN),
        )
        .await
        .unwrap();

    let person = |subject: &str, roles: Vec<Role>| Principal {
        subject: subject.into(),
        handle: String::new(),
        display_name: None,
        roles,
    };
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential("root-code", person("root", vec![Role::Owner]))
            .with_credential("acme-code", person("acme-admin", vec![]))
            .with_credential("acme-reader-code", person("acme-reader", vec![]))
            .with_credential("globex-code", person("globex-admin", vec![])),
    );
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(T0)), logs)
            .with_auth(auth, Arc::new(InMemorySessions::new()))
            .with_rbac(rbac)
            .with_api_tokens(tokens)
            .with_secrets(secrets)
            .with_forge(Arc::new(forge))
            .with_forge_connections(db.clone())
            .with_github_webhook_secret(WEBHOOK_SECRET.to_vec()),
    );
    Harness { app, db }
}

/// Deliver a signed GitHub `installation` event: installation `id` on the
/// account `login` whose immutable GitHub id is `account_id`, covering
/// `repos` (`owner/name`).
async fn deliver_installation(
    h: &Harness,
    delivery: &str,
    id: u64,
    login: &str,
    account_id: u64,
    repos: &[&str],
) -> StatusCode {
    let repositories: Vec<serde_json::Value> = repos
        .iter()
        .map(|r| serde_json::json!({ "full_name": r }))
        .collect();
    let body = serde_json::to_vec(&serde_json::json!({
        "action": "created",
        "installation": { "id": id, "account": { "login": login, "id": account_id } },
        "repositories": repositories,
    }))
    .unwrap();
    let sig = scarab_forge_github::sign_hex(WEBHOOK_SECRET, &body);
    let req = Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "installation")
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", &sig)
        .body(Body::from(body))
        .unwrap();
    h.app.clone().oneshot(req).await.unwrap().status()
}

/// Bind `r` through `conn_id` although the connection serves another Org,
/// as an old re-sync, or a config whose `org:` changed, could leave it: the
/// connection is unassigned for the bind and assigned again after.
async fn misbind(h: &Harness, conn_id: &str, r: &RepoRef) {
    let c = h.db.get_connection(conn_id).await.unwrap().unwrap();
    h.db.put_connection(&ForgeConnection {
        org: None,
        ..c.clone()
    })
    .await
    .unwrap();
    h.db.bind_repo(conn_id, r, &r.owner, &r.name).await.unwrap();
    h.db.put_connection(&c).await.unwrap();
}

async fn login(app: &axum::Router, credential: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "credential": credential }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    v["session"].as_str().unwrap().to_string()
}

async fn call(
    h: &Harness,
    method: &str,
    uri: &str,
    bearer: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"));
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, v)
}

fn ids(list: &serde_json::Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect()
}

/// Every connection-scoped route, aimed at acme's connection.
fn routes_on_acme() -> Vec<(&'static str, &'static str, Option<serde_json::Value>)> {
    vec![
        ("GET", "/v1/connections/gh-acme/preflight", None),
        ("GET", "/v1/connections/gh-acme/available-repos", None),
        ("POST", "/v1/connections/gh-acme/resync", None),
        (
            "POST",
            "/v1/connections/gh-acme/repos",
            Some(serde_json::json!({ "owner": "acme", "name": "api" })),
        ),
        (
            "POST",
            "/v1/connections/gh-acme/repos/acme/web/webhook",
            None,
        ),
        ("DELETE", "/v1/connections/gh-acme/repos/acme/web", None),
        ("DELETE", "/v1/connections/gh-acme?unbind_repos=true", None),
    ]
}

/// acme's connection, its one Project and nothing more, as the harness made them.
async fn assert_acme_untouched(h: &Harness) {
    let c = h.db.get_connection("gh-acme").await.unwrap();
    assert!(c.is_some(), "acme's connection was deleted");
    assert_eq!(
        h.db.repos_of("gh-acme").await.unwrap(),
        vec![repo("acme", "web")],
        "acme's bindings changed"
    );
}

fn forge() -> FakeForge {
    FakeForge::new().with_accessible_repos(&[
        ("acme", "web"),
        ("acme", "api"),
        ("globex", "site"),
        ("globex", "other"),
    ])
}

#[tokio::test]
async fn an_admin_of_another_org_neither_sees_nor_acts_on_a_connection() {
    let h = harness(forge()).await;
    let globex = login(&h.app, "globex-code").await;

    let (st, list) = call(&h, "GET", "/v1/connections", &globex, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ids(&list), vec!["fj-globex"], "globex sees its own only");
    let (_, filtered) = call(&h, "GET", "/v1/connections?org=acme", &globex, None).await;
    assert_eq!(ids(&filtered), Vec::<String>::new());

    for (method, uri, body) in routes_on_acme() {
        let (st, b) = call(&h, method, uri, &globex, body).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{method} {uri}: {b}");
    }
    assert_acme_untouched(&h).await;
    assert!(h.db.resolve(&repo("acme", "api")).await.unwrap().is_none());

    // The unassigned installation is a global admin's alone.
    let (st, _) = call(
        &h,
        "POST",
        "/v1/connections/gh-legacy/resync",
        &globex,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // acme's admin sees and acts on acme's, and not on globex's.
    let acme = login(&h.app, "acme-code").await;
    let (_, list) = call(&h, "GET", "/v1/connections", &acme, None).await;
    assert_eq!(ids(&list), vec!["gh-acme"]);
    let (st, _) = call(
        &h,
        "GET",
        "/v1/connections/gh-acme/available-repos",
        &acme,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(
        &h,
        "DELETE",
        "/v1/connections/fj-globex?unbind_repos=true",
        &acme,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(h.db.get_connection("fj-globex").await.unwrap().is_some());

    // A global admin sees all three, the unassigned one included.
    let root = login(&h.app, "root-code").await;
    let (_, list) = call(&h, "GET", "/v1/connections", &root, None).await;
    assert_eq!(ids(&list), vec!["fj-globex", "gh-acme", "gh-legacy"]);
    assert_eq!(list[2]["org"], serde_json::Value::Null);
}

/// An API token scoped to globex used to pass the "admin of some org" gate
/// and then act on any connection.
#[tokio::test]
async fn an_org_scoped_api_token_is_held_to_its_org() {
    let h = harness(forge()).await;

    let (st, list) = call(&h, "GET", "/v1/connections", GLOBEX_TOKEN, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ids(&list), vec!["fj-globex"]);
    for (method, uri, body) in routes_on_acme() {
        let (st, b) = call(&h, method, uri, GLOBEX_TOKEN, body).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{method} {uri}: {b}");
    }
    assert_acme_untouched(&h).await;

    let (st, _) = call(
        &h,
        "POST",
        "/v1/connections",
        GLOBEX_TOKEN,
        Some(serde_json::json!({
            "org": "acme", "kind": "forgejo", "base_url": "https://git.acme.test", "credential": "t"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_connection_is_created_only_for_an_org_the_caller_administers() {
    let h = harness(forge()).await;
    let globex = login(&h.app, "globex-code").await;
    let create = |org: &str| {
        serde_json::json!({
            "org": org, "kind": "forgejo", "base_url": "https://git.example.test", "credential": "t"
        })
    };

    let (st, _) = call(&h, "POST", "/v1/connections", &globex, Some(create("acme"))).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "another org's");
    // An Org that does not exist yet: its first connection needs a global admin.
    let (st, _) = call(
        &h,
        "POST",
        "/v1/connections",
        &globex,
        Some(create("newco")),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a new org");
    let (st, b) = call(
        &h,
        "POST",
        "/v1/connections",
        &globex,
        Some(create("globex")),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let made =
        h.db.get_connection(b["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(made.org.as_deref(), Some("globex"));

    let root = login(&h.app, "root-code").await;
    let (st, b) = call(&h, "POST", "/v1/connections", &root, Some(create("newco"))).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    // `/v1/me` tells the UI who may take that path.
    let (_, me) = call(&h, "GET", "/v1/me", &root, None).await;
    assert_eq!(me["global_admin"], true);
    let (_, me) = call(&h, "GET", "/v1/me", &globex, None).await;
    assert_eq!(me["global_admin"], false);
    assert_eq!(me["admin_orgs"], serde_json::json!(["globex"]));
}

/// The (kind, base URL) uniqueness is per Org: two Orgs on one host each
/// have a connection; one Org twice is the double-submit it always caught.
#[tokio::test]
async fn one_host_serves_two_orgs_but_not_one_org_twice() {
    let h = harness(forge()).await;
    let root = login(&h.app, "root-code").await;
    let create = |org: &str| {
        serde_json::json!({
            "org": org, "kind": "forgejo", "base_url": "https://codeberg.org", "credential": "t"
        })
    };
    let (st, _) = call(&h, "POST", "/v1/connections", &root, Some(create("acme"))).await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, _) = call(&h, "POST", "/v1/connections", &root, Some(create("globex"))).await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, _) = call(&h, "POST", "/v1/connections", &root, Some(create("acme"))).await;
    assert_eq!(st, StatusCode::CONFLICT);
}

/// A connection binds only its own Org's repos: a repo of another owner,
/// bound or not, is refused, even by a global admin.
#[tokio::test]
async fn a_bind_refuses_a_repo_the_connections_org_does_not_own() {
    let h = harness(forge()).await;
    for who in ["acme-code", "root-code"] {
        let s = login(&h.app, who).await;
        for name in ["other", "site"] {
            let (st, b) = call(
                &h,
                "POST",
                "/v1/connections/gh-acme/repos",
                &s,
                Some(serde_json::json!({ "owner": "globex", "name": name, "register_webhook": false })),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{who} globex/{name}: {b}");
        }
    }
    assert!(h
        .db
        .resolve(&repo("globex", "other"))
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        h.db.resolve(&repo("globex", "site"))
            .await
            .unwrap()
            .unwrap()
            .connection
            .id,
        "fj-globex"
    );
}

/// Re-sync reports what the credential reaches as available, but only the
/// connection's own Org's repos, binds none of them, and never moves a repo
/// bound through another Org's connection. An unassigned connection reports
/// nothing available on a re-sync.
#[tokio::test]
async fn resync_never_binds_or_moves_another_orgs_repo() {
    let h = harness(forge()).await;
    let root = login(&h.app, "root-code").await;

    let (st, b) = call(&h, "POST", "/v1/connections/gh-acme/resync", &root, None).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["available"], serde_json::json!(["acme/api"]));
    assert_eq!(b["moved"], serde_json::json!([]));
    assert!(
        h.db.resolve(&repo("acme", "api")).await.unwrap().is_none(),
        "available is not enabled"
    );
    let mut skipped: Vec<&str> = b["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    skipped.sort();
    assert_eq!(skipped, vec!["globex/other", "globex/site"]);

    // The unassigned installation binds nothing, and the repos already
    // bound in acme and globex stay where they are.
    let (st, b) = call(&h, "POST", "/v1/connections/gh-legacy/resync", &root, None).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    for (r, holder) in [
        (repo("acme", "web"), "gh-acme"),
        (repo("globex", "site"), "fj-globex"),
    ] {
        assert_eq!(
            h.db.resolve(&r).await.unwrap().unwrap().connection.id,
            holder,
            "{}/{} was moved",
            r.owner,
            r.name
        );
    }
}

/// A re-sync through an unassigned connection binds nothing and does not
/// assign it: its repos would be homed by owner name alone, which is how a
/// GitHub account joins a same-named Org of another forge (D7). A global
/// admin assigns it first (`PUT .../org`).
///
/// (That a read such as `available-repos` assigns nothing is not tested
/// here: the code that assigned on a read asked the forge for the
/// connection's account, a call the forge port no longer has, so no fake
/// can make that half fail.)
#[tokio::test]
async fn an_unassigned_connection_is_not_assigned_or_bound_by_a_resync() {
    let h =
        harness(FakeForge::new().with_accessible_repos(&[("umbrella", "app"), ("acme", "new")]))
            .await;
    let root = login(&h.app, "root-code").await;

    let (st, b) = call(&h, "POST", "/v1/connections/gh-legacy/resync", &root, None).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["available"], serde_json::json!([]));
    assert_eq!(b["moved"], serde_json::json!([]));
    let mut skipped: Vec<&str> = b["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    skipped.sort();
    assert_eq!(skipped, vec!["acme/new", "umbrella/app"]);

    let c = h.db.get_connection("gh-legacy").await.unwrap().unwrap();
    assert_eq!(c.org, None, "still unassigned");
    assert!(h.db.repos_of("gh-legacy").await.unwrap().is_empty());
}

/// A repo moves only off an unassigned connection. Manual bind and re-sync
/// give the same answer as the registry: a second connection of the same
/// Org does not take a repo its first one holds (409 / `skipped`), and a
/// repo held by an unassigned (legacy) connection moves to its Org's own.
#[tokio::test]
async fn a_repo_moves_only_off_an_unassigned_connection() {
    let h = harness(forge()).await;
    h.db.put_connection(&conn(
        "fj-acme",
        ForgeKind::Forgejo,
        "https://git.acme.test",
        Some("acme"),
        None,
    ))
    .await
    .unwrap();
    // A legacy row that holds an acme repo.
    h.db.bind_repo("gh-legacy", &repo("acme", "api"), "acme", "api")
        .await
        .unwrap();
    let acme = login(&h.app, "acme-code").await;

    // Off the unassigned connection: an acme admin's bind moves it.
    let (st, b) = call(
        &h,
        "POST",
        "/v1/connections/gh-acme/repos",
        &acme,
        Some(serde_json::json!({ "owner": "acme", "name": "api", "register_webhook": false })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let moved = h.db.resolve(&repo("acme", "api")).await.unwrap().unwrap();
    assert_eq!(moved.connection.id, "gh-acme");
    assert_eq!(moved.org, "acme");

    // Same Org, another connection: refused by bind, skipped by re-sync.
    let (st, b) = call(
        &h,
        "POST",
        "/v1/connections/fj-acme/repos",
        &acme,
        Some(serde_json::json!({ "owner": "acme", "name": "web", "register_webhook": false })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    let (st, b) = call(&h, "POST", "/v1/connections/fj-acme/resync", &acme, None).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert!(
        b["skipped"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("acme/web")),
        "{b}"
    );
    assert_eq!(
        h.db.resolve(&repo("acme", "web"))
            .await
            .unwrap()
            .unwrap()
            .connection
            .id,
        "gh-acme",
        "acme/web stayed where it was"
    );
}

/// A connection that serves an Org governs that Org's repos only: a row
/// binding another owner's repo through it (an old re-sync, a hand edit)
/// is not its admin's to remove.
#[tokio::test]
async fn an_org_admin_cannot_unbind_another_owners_repo_from_its_connection() {
    let h = harness(forge()).await;
    misbind(&h, "gh-acme", &repo("globex", "other")).await;
    let acme = login(&h.app, "acme-code").await;
    let (st, b) = call(
        &h,
        "DELETE",
        "/v1/connections/gh-acme/repos/globex/other",
        &acme,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(
        h.db.resolve(&repo("globex", "other"))
            .await
            .unwrap()
            .unwrap()
            .org,
        "globex"
    );
}

/// `PUT /v1/connections/{id}/org`: a global admin's explicit act, refused
/// to anyone else, and refused (409) when a repo bound through the
/// connection is not the Org's or the Org is served by another forge.
#[tokio::test]
async fn only_a_global_admin_assigns_a_connection_and_only_to_its_repos_org() {
    let h = harness(forge()).await;
    h.db.bind_repo("gh-legacy", &repo("umbrella", "app"), "umbrella", "app")
        .await
        .unwrap();
    let put = |org: &str| Some(serde_json::json!({ "org": org }));
    let uri = "/v1/connections/gh-legacy/org";

    let acme = login(&h.app, "acme-code").await;
    let (st, b) = call(&h, "PUT", uri, &acme, put("acme")).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "an org admin: {b}");
    let (st, _) = call(&h, "PUT", uri, GLOBEX_TOKEN, put("globex")).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "an org-scoped token");

    let root = login(&h.app, "root-code").await;
    // A bound repo of another owner.
    let (st, b) = call(&h, "PUT", uri, &root, put("acme")).await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert!(b.as_str().unwrap().contains("umbrella/app"), "{b}");
    // Not an org name.
    // Nor `.`/`..` (a URL cannot address them), a control character, or a
    // first segment of the web UI's addresses.
    for bad in [
        "", "a/b", "a b", "_forge", ".", "..", "a\u{7}b", "settings", "orgs", "account", "security",
    ] {
        let (st, b) = call(&h, "PUT", uri, &root, put(bad)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad:?}: {b}");
    }
    assert_eq!(
        h.db.get_connection("gh-legacy").await.unwrap().unwrap().org,
        None
    );

    // The Org of its repos: assigned, and its admin then sees it.
    let (st, b) = call(&h, "PUT", uri, &root, put("umbrella")).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["org"], "umbrella");
    assert_eq!(
        h.db.get_connection("gh-legacy")
            .await
            .unwrap()
            .unwrap()
            .org
            .as_deref(),
        Some("umbrella")
    );

    // D7 again: globex is served by a Forgejo connection, so a GitHub
    // connection with nothing bound is not attached to it by name.
    h.db.put_connection(&conn(
        "github-install-8",
        ForgeKind::GitHub,
        "https://api.github.com",
        None,
        Some(8),
    ))
    .await
    .unwrap();
    let (st, b) = call(
        &h,
        "PUT",
        "/v1/connections/github-install-8/org",
        &root,
        put("globex"),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert_eq!(
        h.db.get_connection("github-install-8")
            .await
            .unwrap()
            .unwrap()
            .org,
        None
    );
}

/// Removing a connection with its Projects takes only its own Org's. A
/// binding of another owner that it holds (an old re-sync, a config whose
/// `org:` changed) is refused, named, and left in place, and the connection
/// stays. That Org takes its repo back by re-syncing its own connection,
/// after which the removal goes through. Before the fix the first removal
/// deleted globex's Project.
#[tokio::test]
async fn removing_a_connection_never_takes_another_orgs_project_with_it() {
    let h = harness(forge()).await;
    misbind(&h, "gh-acme", &repo("globex", "other")).await;
    let acme = login(&h.app, "acme-code").await;

    let (st, b) = call(
        &h,
        "DELETE",
        "/v1/connections/gh-acme?unbind_repos=true",
        &acme,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert!(b.as_str().unwrap().contains("globex/other"), "{b}");
    assert!(h.db.get_connection("gh-acme").await.unwrap().is_some());
    assert_eq!(
        h.db.resolve(&repo("globex", "other"))
            .await
            .unwrap()
            .unwrap()
            .connection
            .id,
        "gh-acme"
    );

    // globex re-syncs its own connection: the repo moves off a connection
    // that does not serve its owner.
    let globex = login(&h.app, "globex-code").await;
    let (st, b) = call(
        &h,
        "POST",
        "/v1/connections/fj-globex/resync",
        &globex,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert!(
        b["moved"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("globex/other")),
        "{b}"
    );
    let moved =
        h.db.resolve(&repo("globex", "other"))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(moved.connection.id, "fj-globex");

    let (st, b) = call(
        &h,
        "DELETE",
        "/v1/connections/gh-acme?unbind_repos=true",
        &acme,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");
    assert!(h.db.resolve(&repo("acme", "web")).await.unwrap().is_none());
    assert_eq!(
        h.db.resolve(&repo("globex", "other"))
            .await
            .unwrap()
            .unwrap()
            .connection
            .id,
        "fj-globex"
    );
}

/// D7 of git-bug 19b0a45, the renamed-login case. acme's installation was
/// recorded with GitHub account id 1. That account renames itself, and a
/// new account (id 2) takes the free login `acme` and installs the App. Its
/// installation is registered unassigned and binds nothing: the login is
/// the same, the account is not. The account with id 1 installing again
/// joins acme, and, as every installation now, binds none of its repos: an
/// admin enables them. Before the fix any GitHub installation named `acme`
/// joined.
#[tokio::test]
async fn an_installation_by_a_new_holder_of_a_renamed_login_does_not_join_its_org() {
    let h = harness(forge()).await;
    h.db.set_connection_account("gh-acme", Some(1), "acme")
        .await
        .unwrap();

    let st = deliver_installation(&h, "rename-1", 9, "acme", 2, &["acme/new"]).await;
    assert_eq!(st, StatusCode::OK);
    let c =
        h.db.get_connection("github-install-9")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(c.org, None, "registered unassigned");
    assert_eq!(c.account_id, Some(2), "its account id is recorded");
    assert!(h.db.repos_of("github-install-9").await.unwrap().is_empty());
    assert!(h.db.resolve(&repo("acme", "new")).await.unwrap().is_none());

    let st = deliver_installation(&h, "rename-2", 10, "acme", 1, &["acme/more"]).await;
    assert_eq!(st, StatusCode::OK);
    let c =
        h.db.get_connection("github-install-10")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(c.org.as_deref(), Some("acme"));
    assert!(h.db.repos_of("github-install-10").await.unwrap().is_empty());
    assert!(h.db.resolve(&repo("acme", "more")).await.unwrap().is_none());
}

/// git-bug bb8d644. A GitHub installation left waiting for an organization
/// shows its account login, account id and installation id in the list, so a
/// global admin can tell two waiting connections apart and create the Org
/// from the login. The login follows a rename on the next delivery; the id
/// does not change.
#[tokio::test]
async fn a_waiting_installation_shows_its_account_and_follows_a_renamed_login() {
    let h = harness(forge()).await;
    // gh-acme serves `acme` as account 1, so account 2 named `acme` waits.
    let st = deliver_installation(&h, "wait-1", 9, "acme", 2, &[]).await;
    assert_eq!(st, StatusCode::OK);
    let root = login(&h.app, "root-code").await;
    let row = |list: &serde_json::Value| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "github-install-9")
            .cloned()
            .expect("the waiting installation is listed")
    };

    let (st, list) = call(&h, "GET", "/v1/connections", &root, None).await;
    assert_eq!(st, StatusCode::OK, "{list}");
    let c = row(&list);
    assert_eq!(c["org"], serde_json::Value::Null, "it waits: {c}");
    assert_eq!(c["account_login"], "acme");
    assert_eq!(c["account_id"], 2);
    assert_eq!(c["installation_id"], 9);

    let st = deliver_installation(&h, "wait-2", 9, "acme-labs", 2, &[]).await;
    assert_eq!(st, StatusCode::OK);
    let (_, list) = call(&h, "GET", "/v1/connections", &root, None).await;
    let c = row(&list);
    assert_eq!(c["account_login"], "acme-labs", "the current login: {c}");
    assert_eq!(c["account_id"], 2);

    // A connection no installation delivery has reached knows neither.
    let fj = list
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "fj-globex")
        .unwrap();
    assert_eq!(fj["account_login"], serde_json::Value::Null);
    assert_eq!(fj["installation_id"], serde_json::Value::Null);
}

/// D7 of git-bug 19b0a45, the data-only case. A global admin sets up
/// `initech` with a secret before it has any connection. A GitHub account
/// named `initech` installing the App does not join it by name: the secret
/// made the Org exist. An account with no Org of its name founds one.
/// Before the fix a secret did not make an Org exist, and the installation
/// took `initech` and its secret.
#[tokio::test]
async fn an_installation_does_not_join_an_org_that_only_holds_data() {
    let h = harness(forge()).await;
    let root = login(&h.app, "root-code").await;
    let (st, b) = call(
        &h,
        "POST",
        "/v1/secrets",
        &root,
        Some(serde_json::json!({ "org": "initech", "name": "DEPLOY_KEY", "value": "v" })),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");

    let st = deliver_installation(&h, "data-1", 12, "initech", 7, &["initech/tps"]).await;
    assert_eq!(st, StatusCode::OK);
    let c =
        h.db.get_connection("github-install-12")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(c.org, None, "registered unassigned");
    assert!(h
        .db
        .resolve(&repo("initech", "tps"))
        .await
        .unwrap()
        .is_none());

    let st = deliver_installation(&h, "data-2", 13, "hooli", 8, &["hooli/app"]).await;
    assert_eq!(st, StatusCode::OK);
    let c =
        h.db.get_connection("github-install-13")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(c.org.as_deref(), Some("hooli"));
}

fn names(page: &serde_json::Value) -> Vec<String> {
    page["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

/// The org's repositories page: every enabled repo and every one its
/// connections reach that is not enabled, one page at a time. Only the
/// org's own repos are listed, an admin may act on each row, a reader may
/// only look, and an admin of another org is refused.
#[tokio::test]
async fn the_org_repos_page_lists_enabled_and_available_repos_to_the_orgs_readers() {
    let h = harness(forge()).await;

    let acme = login(&h.app, "acme-code").await;
    let (st, page) = call(&h, "GET", "/v1/orgs/acme/repos", &acme, None).await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(names(&page), vec!["api", "web"], "sorted by name: {page}");
    assert_eq!(page["total"], 2);
    assert_eq!(page["enabled_total"], 1);
    assert_eq!(page["can_administer"], true);
    let api = &page["repos"][0];
    assert_eq!(api["enabled"], false);
    assert!(api["project"].is_null());
    assert_eq!(api["connection_id"], "gh-acme");
    assert_eq!(api["can_change"], true);
    assert_eq!(api["repo_url"], "https://github.com/acme/api");
    let web = &page["repos"][1];
    assert_eq!(web["enabled"], true);
    assert_eq!(web["project"], "web");
    assert_eq!(web["can_change"], true);
    assert_eq!(page["unlisted"], serde_json::json!([]));

    // Paged on the server, and filtered by name before paging.
    let (_, p2) = call(
        &h,
        "GET",
        "/v1/orgs/acme/repos?per_page=1&page=2",
        &acme,
        None,
    )
    .await;
    assert_eq!(names(&p2), vec!["web"]);
    assert_eq!(p2["total"], 2);
    assert_eq!(p2["page"], 2);
    assert_eq!(p2["per_page"], 1);
    let (_, q) = call(&h, "GET", "/v1/orgs/acme/repos?q=AP", &acme, None).await;
    assert_eq!(names(&q), vec!["api"]);
    assert_eq!(q["total"], 1);
    assert_eq!(
        q["enabled_total"], 1,
        "the enabled count ignores the filter"
    );

    // A reader sees the same list and may act on none of it.
    let reader = login(&h.app, "acme-reader-code").await;
    let (st, page) = call(&h, "GET", "/v1/orgs/acme/repos", &reader, None).await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(names(&page), vec!["api", "web"]);
    assert_eq!(page["can_administer"], false);
    assert!(page["repos"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["can_change"] == false));

    // Another org's admin is refused, by session and by org token.
    let globex = login(&h.app, "globex-code").await;
    for bearer in [globex.as_str(), GLOBEX_TOKEN] {
        let (st, _) = call(&h, "GET", "/v1/orgs/acme/repos", bearer, None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }
}

/// `?connection=` narrows the org's repositories page to one connection:
/// what is enabled through it and what only it reaches. The enabled count
/// stays the org's. The Connections page links each connection here.
#[tokio::test]
async fn the_org_repos_page_filters_by_connection() {
    let h = harness(forge()).await;
    // A second acme connection, with a repo of its own.
    h.db.put_connection(&conn(
        "fj-acme",
        ForgeKind::Forgejo,
        "https://git.acme.test",
        Some("acme"),
        None,
    ))
    .await
    .unwrap();
    h.db.bind_repo("fj-acme", &repo("acme", "infra"), "acme", "infra")
        .await
        .unwrap();
    let acme = login(&h.app, "acme-code").await;

    let (st, page) = call(
        &h,
        "GET",
        "/v1/orgs/acme/repos?connection=fj-acme",
        &acme,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    // web is enabled through gh-acme, so it is not fj-acme's even though
    // fj-acme's forge reaches it.
    assert_eq!(names(&page), vec!["api", "infra"], "{page}");
    assert!(page["repos"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["connection_id"] == "fj-acme"));
    assert_eq!(page["total"], 2);
    assert_eq!(page["enabled_total"], 2, "the org's, whatever the filter");

    let (_, page) = call(
        &h,
        "GET",
        "/v1/orgs/acme/repos?connection=gh-acme",
        &acme,
        None,
    )
    .await;
    assert_eq!(names(&page), vec!["api", "web"], "{page}");
    assert!(page["repos"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["connection_id"] == "gh-acme"));

    // A connection of another org lists nothing of acme's.
    let (st, page) = call(
        &h,
        "GET",
        "/v1/orgs/acme/repos?connection=fj-globex",
        &acme,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(page["total"], 0, "{page}");
}

/// Disabling a GitHub repo is permanent: the unbind removes its Project, and
/// neither a later re-sync nor a later delivery of the installation binds
/// it again. The repo is listed as available instead.
#[tokio::test]
async fn a_disabled_repo_stays_disabled_after_a_resync() {
    let h = harness(forge()).await;
    let acme = login(&h.app, "acme-code").await;

    let (st, b) = call(
        &h,
        "DELETE",
        "/v1/connections/gh-acme/repos/acme/web",
        &acme,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");

    let (st, b) = call(&h, "POST", "/v1/connections/gh-acme/resync", &acme, None).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert!(
        b["available"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("acme/web")),
        "{b}"
    );
    assert!(h.db.resolve(&repo("acme", "web")).await.unwrap().is_none());

    let (_, page) = call(&h, "GET", "/v1/orgs/acme/repos", &acme, None).await;
    assert_eq!(page["enabled_total"], 0, "{page}");
    assert!(page["repos"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "web" && r["enabled"] == false));
}
