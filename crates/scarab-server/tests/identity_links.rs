//! Account linking (ADR-0069 §6) end to end: the real router, two stub login
//! providers over real HTTP (the one true external), and *real* Postgres —
//! because a merge rewrites bindings, approver lists, tokens and sessions in
//! ONE transaction, and only the Postgres adapter can do that.
//!
//! The story: Ada signs in with Okta. An admin had already imported her GitHub
//! account from the forge, so it exists as an UNCLAIMED Principal (`ada-gh`)
//! holding forge roles and an approver entry. When Ada links her GitHub
//! account, the OAuth round-trip proves she controls `ada-gh`'s only way in,
//! so `ada-gh` is folded into her — without ever matching anything by email.
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::Db;
use scarab_identity::{IdentityStore, ProviderIdentity, RbacStore, Role, Scope};
use scarab_project::{EnvironmentStore, ProtectionRules};
use scarab_secrets::SecretScope;
use scarab_server::config::OAuthConfig;
use scarab_server::oauth::OAuthAuthenticator;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryObjectStore};

/// A stub provider: `good-code` → an access token; `/user` answers whatever
/// the test currently says the signed-in account is.
#[derive(Clone)]
struct Stub {
    base: String,
    user: Arc<Mutex<serde_json::Value>>,
}

async fn stub(user: serde_json::Value) -> Stub {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let s = Stub {
        base: format!("http://{}", listener.local_addr().unwrap()),
        user: Arc::new(Mutex::new(user)),
    };
    let u = s.user.clone();
    let app = axum::Router::new()
        .route(
            "/token",
            post(|body: String| async move {
                if !body.contains("code=good-code") {
                    return (StatusCode::BAD_REQUEST, "bad code").into_response();
                }
                axum::Json(serde_json::json!({ "access_token": "at", "token_type": "bearer" }))
                    .into_response()
            }),
        )
        .route(
            "/user",
            get(move || {
                let u = u.clone();
                async move { axum::Json(u.lock().unwrap().clone()) }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    s
}

fn provider(
    s: &Stub,
    id: &str,
    unqualified: bool,
    pg: &Arc<PostgresDb>,
) -> Arc<OAuthAuthenticator> {
    Arc::new(
        OAuthAuthenticator::new(OAuthConfig {
            client_id: "cid".into(),
            client_secret: "sekret".into(),
            authorize_url: format!("{}/authorize", s.base),
            token_url: format!("{}/token", s.base),
            userinfo_url: format!("{}/user", s.base),
            oidc_issuer: None,
            scopes: String::new(),
            // Only the pre-merge GitHub id is an owner: a merge must carry it.
            owners: vec!["ada-gh".into()],
            provider_id: id.into(),
            unqualified_ids: unqualified,
            name: id.into(),
            forge_url: unqualified.then(|| "https://api.github.com".into()),
        })
        .with_identities(pg.clone()),
    )
}

/// Cookies a browser holds for this host.
#[derive(Default, Clone)]
struct Jar(Vec<(String, String)>);

impl Jar {
    fn absorb(&mut self, resp: &axum::response::Response) {
        for v in resp.headers().get_all(axum::http::header::SET_COOKIE) {
            let v = v.to_str().unwrap();
            let (kv, _) = v.split_once(';').unwrap_or((v, ""));
            let (k, val) = kv.split_once('=').unwrap();
            self.0.retain(|(name, _)| name != k);
            if !v.contains("Max-Age=0") {
                self.0.push((k.to_string(), val.to_string()));
            }
        }
    }
    fn get(&self, name: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }
    fn header(&self) -> String {
        self.0
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

async fn call(
    app: &axum::Router,
    jar: &mut Jar,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> axum::response::Response {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", jar.header())
        .header("content-type", "application/json");
    if let Some(csrf) = jar.get("scarab_csrf") {
        req = req.header("x-csrf-token", csrf);
    }
    let resp = app
        .clone()
        .oneshot(
            req.body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    jar.absorb(&resp);
    resp
}

/// The state value the server planted (first cookie component).
fn state_of(jar: &Jar) -> String {
    jar.get("scarab_oauth_state")
        .unwrap()
        .split('.')
        .next()
        .unwrap()
        .to_string()
}

async fn browser_sign_in(app: &axum::Router, provider: &str) -> Jar {
    let mut jar = Jar::default();
    let resp = call(
        app,
        &mut jar,
        "GET",
        &format!("/v1/auth/login?provider={provider}"),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let state = state_of(&jar);
    let resp = call(
        app,
        &mut jar,
        "GET",
        &format!("/v1/auth/callback?code=good-code&state={state}"),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(jar.get("scarab_session").is_some());
    jar
}

async fn me(app: &axum::Router, jar: &mut Jar) -> serde_json::Value {
    let resp = call(app, jar, "GET", "/v1/me", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Begin a link as `jar`, returning the state the callback must echo.
async fn begin_link(app: &axum::Router, jar: &mut Jar, provider: &str) -> String {
    let resp = call(
        app,
        jar,
        "POST",
        "/v1/me/identities/link",
        Some(serde_json::json!({ "provider": provider })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    state_of(jar)
}

fn web() -> Scope {
    Scope::Project {
        org: "acme".into(),
        name: "web".into(),
    }
}

fn api() -> Scope {
    Scope::Project {
        org: "acme".into(),
        name: "api".into(),
    }
}

#[tokio::test]
async fn linking_proves_and_merges_an_imported_account_without_email() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();

    let github = stub(serde_json::json!({ "id": 1001, "login": "ada-gh" })).await;
    let okta = stub(serde_json::json!({
        "sub": "00u-ada", "email": "ada@example.com", "email_verified": true
    }))
    .await;
    let gh = provider(&github, "default", true, &pg);
    let ok = provider(&okta, "okta", false, &pg);
    let db: Arc<dyn Db> = pg.clone();
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    let app = router(
        AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
            .with_public_url("https://scarab.example.com")
            .with_auth(gh.clone(), pg.clone())
            .with_oauth_login(gh)
            .with_oauth_login(ok)
            .with_rbac(pg.clone())
            .with_identities(pg.clone())
            .with_environments(pg.clone()),
    );

    // An admin imported Ada's GitHub account before she ever signed in: an
    // UNCLAIMED Principal with forge roles and an approver entry.
    let gh_ada = ProviderIdentity {
        provider: "default".into(),
        external_id: "1001".into(),
        handle: "ada-gh".into(),
        email: None,
        email_verified: false,
        display_name: None,
    };
    assert_eq!(pg.pin(&gh_ada, "ada-gh", 1).await.unwrap(), "ada-gh");
    pg.import_seeds(&web(), "default", &[("ada-gh".into(), Role::Admin)], false)
        .await
        .unwrap();
    pg.import_seeds(&api(), "default", &[("ada-gh".into(), Role::Member)], false)
        .await
        .unwrap();
    pg.put_environment(
        "acme",
        "web",
        &scarab_project::Environment {
            name: "prod".into(),
            protection: ProtectionRules {
                approvers: vec!["ada-gh".into()],
                wait_timer: 0,
                allowed_refs: vec![],
                concurrency: 1,
                secret_scope: SecretScope::Environment {
                    org: "acme".into(),
                    repo: "web".into(),
                    environment: "prod".into(),
                },
                oidc_subject: "scarab:org/acme/repo/web/env/prod".into(),
                privileged_images: vec![],
                permit_k8s_overlay: false,
                require_reason: false,
            },
        },
    )
    .await
    .unwrap();

    // Ada signs in with Okta: a separate Principal, keyed by her verified email
    // handle — and NOT linked to the GitHub one, though nothing stops an email
    // match from looking tempting.
    let mut ada = browser_sign_in(&app, "okta").await;
    let before = me(&app, &mut ada).await;
    assert_eq!(before["subject"], "okta:ada@example.com");
    assert_eq!(before["roles"], serde_json::json!(["Viewer"]));
    let ada_id = "okta:ada@example.com";
    // A deliberate native downgrade the merge must not undo.
    pg.grant(
        &scarab_identity::Binding {
            subject: ada_id.into(),
            scope: web(),
            role: Role::Viewer,
        },
        scarab_identity::BindingOrigin::Native,
    )
    .await
    .unwrap();

    // A link started in Ada's session cannot be completed in Bob's.
    *github.user.lock().unwrap() = serde_json::json!({ "id": 2002, "login": "bob" });
    let mut bob2 = browser_sign_in(&app, "default").await;
    *github.user.lock().unwrap() = serde_json::json!({ "id": 1001, "login": "ada-gh" });
    let state = begin_link(&app, &mut ada, "default").await;
    let mut hijack = bob2.clone();
    hijack.0.retain(|(k, _)| k != "scarab_oauth_state");
    hijack.0.push((
        "scarab_oauth_state".into(),
        ada.get("scarab_oauth_state").unwrap(),
    ));
    let resp = call(
        &app,
        &mut hijack,
        "GET",
        &format!("/v1/auth/callback?code=good-code&state={state}"),
        None,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "bound to the starting session"
    );

    // Ada completes the link herself: the GitHub round-trip proves `ada-gh`'s
    // ONLY identity, so `ada-gh` is folded into her.
    let resp = call(
        &app,
        &mut ada,
        "GET",
        &format!("/v1/auth/callback?code=good-code&state={state}"),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        resp.headers()["location"],
        "/account?identity_linked=default&merged=ada-gh"
    );
    let folded = pg.principal("ada-gh").await.unwrap().unwrap();
    assert_eq!(folded.merged_into.as_deref(), Some(ada_id));
    assert_eq!(
        pg.role_of(ada_id, &web()).await.unwrap(),
        Some(Role::Viewer),
        "her native Viewer beats the imported Admin"
    );
    assert_eq!(
        pg.role_of(ada_id, &api()).await.unwrap(),
        Some(Role::Member)
    );
    assert_eq!(pg.role_of("ada-gh", &api()).await.unwrap(), None);
    let env = pg
        .get_environment("acme", "web", "prod")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(env.protection.approvers, vec![ada_id.to_string()]);
    let linked = me(&app, &mut ada).await;
    assert_eq!(
        linked["identities"],
        serde_json::json!([
            { "provider": "default", "handle": "ada-gh" },
            { "provider": "okta", "handle": "ada@example.com" },
        ])
    );

    // Signing in with GitHub now lands on Ada — and the owners entry naming
    // the folded id still means her.
    let mut via_github = browser_sign_in(&app, "default").await;
    let after = me(&app, &mut via_github).await;
    assert_eq!(after["subject"], ada_id);
    assert_eq!(after["roles"], serde_json::json!(["Owner"]));

    // Bob cannot take that GitHub account: its Principal (Ada) signs in two
    // ways, so proving one does not prove he is Ada.
    let state = begin_link(&app, &mut bob2, "default").await;
    let resp = call(
        &app,
        &mut bob2,
        "GET",
        &format!("/v1/auth/callback?code=good-code&state={state}"),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let loc = resp.headers()["location"].to_str().unwrap().to_string();
    assert!(loc.starts_with("/account?identity_link_error="), "{loc}");
    assert_eq!(
        pg.principal("bob").await.unwrap().unwrap().merged_into,
        None
    );

    // Unlink GitHub: the roles imported through it go with it; her native
    // grant stays. The last way in cannot be unlinked.
    let resp = call(&app, &mut ada, "DELETE", "/v1/me/identities/default", None).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(pg.role_of(ada_id, &api()).await.unwrap(), None);
    assert_eq!(
        pg.role_of(ada_id, &web()).await.unwrap(),
        Some(Role::Viewer)
    );
    let resp = call(&app, &mut ada, "DELETE", "/v1/me/identities/okta", None).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // A cross-site page cannot start a link: no CSRF token, no flow.
    let mut no_csrf = ada.clone();
    no_csrf.0.retain(|(k, _)| k != "scarab_csrf");
    let resp = call(
        &app,
        &mut no_csrf,
        "POST",
        "/v1/me/identities/link",
        Some(serde_json::json!({ "provider": "default" })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    tdb.cleanup().await;
}
