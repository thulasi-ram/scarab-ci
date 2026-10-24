//! RED TEAM — ADR-0073 step-up review (2026-10-24), server side.
//!
//! Each test exercises ONE edge case against step-up enforcement and asserts
//! the secure outcome; a FAILING test means the invariant did not hold. The
//! software passkey and a stub OAuth provider are the only external fakes; the
//! rest is the real router over the in-memory stores (Postgres where noted,
//! skipped without `SCARAB_TEST_DATABASE_URL`).

mod step_up_harness;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{json, Value};

use scarab_identity::{
    api_token_hash, ApiToken, ApiTokenStore, IdentityStore, ProviderIdentity, Role, Scope, Session,
    SessionStore, LOGIN_KIND_BROWSER, STEP_UP_WINDOW_MS, WEBAUTHN_CHALLENGE_TTL_MS,
};
use scarab_project::{
    EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProtectionRules,
};
use scarab_secrets::{SecretProvider, SecretScope};
use scarab_server::config::OAuthConfig;
use scarab_server::oauth::OAuthAuthenticator;
use scarab_testkit::{FakeSecrets, InMemoryEnvironmentGovernance, InMemoryIdentities};
use scarab_webauthn::soft::SoftPasskey;

use step_up_harness::*;

const MIN: i64 = 60 * 1000;
const DAY: i64 = 24 * 60 * MIN;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn with_secrets() -> (Harness, Arc<FakeSecrets>) {
    let secrets = Arc::new(FakeSecrets::new());
    let wired = secrets.clone();
    (harness_with(move |st| st.with_secrets(wired)), secrets)
}

async fn secret_value(secrets: &FakeSecrets) -> Option<String> {
    secrets
        .get(&SecretScope::Org { org: "acme".into() }, "DB_PASSWORD")
        .await
        .ok()
        .map(|s| String::from_utf8(s.value).unwrap())
}

fn secret(value: &str) -> Value {
    json!({"org": "acme", "name": "DB_PASSWORD", "value": value})
}

/// amy: Owner of `acme`, in her browser, with a passkey.
async fn amy(h: &Harness) -> (Sess, SoftPasskey) {
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (s, soft)
}

async fn pat(h: &Harness, owner: &str, id: &str, scope: Scope, role: Role) -> String {
    let plaintext = format!("scarab_pat_{id}_0123456789abcdef0123456789abcdef");
    let now = h.now().await;
    h.tokens
        .put(
            &ApiToken {
                id: id.into(),
                name: format!("ci-{id}"),
                owner_subject: owner.into(),
                scope,
                role,
                expires_at: now + 30 * DAY,
                created_by: owner.into(),
                created_at: now,
                last_used_at: None,
                revoked_at: None,
            },
            &api_token_hash(&plaintext),
        )
        .await
        .unwrap();
    plaintext
}

/// A machine call that must park a request; its id.
async fn raise(h: &Harness, method: &str, uri: &str, body: Value, bearer: &str) -> String {
    let (st, b) = h.bearer(method, uri, Some(body), bearer).await;
    assert!(is_step_up(st, &b), "want a parked request, got {st}: {b}");
    b["request_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a request id: {b}"))
        .to_string()
}

/// A stub OAuth provider (the one external): `good-code` → a token; `/user`
/// answers `user`.
async fn stub_provider(user: Value) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route(
            "/token",
            post(|body: String| async move {
                if !body.contains("code=good-code") {
                    return (StatusCode::BAD_REQUEST, "bad code").into_response();
                }
                axum::Json(json!({ "access_token": "at", "token_type": "bearer" })).into_response()
            }),
        )
        .route(
            "/user",
            get(move || {
                let u = user.clone();
                async move { axum::Json(u) }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn oauth_provider(base: &str, ids: Arc<dyn IdentityStore>) -> Arc<OAuthAuthenticator> {
    Arc::new(
        OAuthAuthenticator::new(OAuthConfig {
            client_id: "cid".into(),
            client_secret: "sekret".into(),
            authorize_url: format!("{base}/authorize"),
            token_url: format!("{base}/token"),
            userinfo_url: format!("{base}/user"),
            oidc_issuer: None,
            scopes: String::new(),
            owners: vec![],
            provider_id: "default".into(),
            unqualified_ids: true,
            name: "default".into(),
            forge_url: Some("https://api.github.com".into()),
        })
        .with_identities(ids),
    )
}

fn identity(provider: &str, external_id: &str, handle: &str) -> ProviderIdentity {
    ProviderIdentity {
        provider: provider.into(),
        external_id: external_id.into(),
        handle: handle.into(),
        email: None,
        email_verified: false,
        display_name: None,
    }
}

/// The value of `name` among a response's `Set-Cookie`s.
fn set_cookie(resp: &axum::response::Response, name: &str) -> Option<String> {
    resp.headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

async fn send_raw(h: &Harness, req: Request<Body>) -> axum::response::Response {
    use tower::ServiceExt;
    h.app.clone().oneshot(req).await.unwrap()
}

// ---------------------------------------------------------------------------
// 1. Identity link (a §3 crown jewel) must not complete from a cookie alone
// ---------------------------------------------------------------------------

/// The attach half of an identity link runs in `GET /v1/auth/callback`, which
/// the guard passes untouched (it is a read). The guarded crown jewel is the
/// `POST /v1/me/identities/link` that plants the link-state cookie. The
/// `scarab_oauth_state` cookie is unsigned, so a holder of a cookie session
/// (no passkey, no window) can start a PLAIN login, append
/// `.<session_hash>` to turn the state into a link flow bound to that session,
/// and complete it through the callback — never calling the guarded POST.
/// SECURE: no second identity is attached to that principal without a step-up.
#[tokio::test]
async fn an_identity_link_cannot_be_completed_without_a_step_up() {
    let base = stub_provider(json!({ "id": 666, "login": "mallory" })).await;
    let ids = Arc::new(InMemoryIdentities::new());
    ids.sign_in(&identity("okta", "amy-sub", "amy"), "amy", T0)
        .await
        .unwrap();
    let provider = oauth_provider(&base, ids.clone() as Arc<dyn IdentityStore>);
    let wired = ids.clone();
    let h = harness_with(move |st| {
        st.with_oauth_login(provider)
            .with_identities(wired as Arc<dyn IdentityStore>)
    });
    let session = h.browser_session(owner("amy")).await;

    // Start a plain login to obtain a valid state/PKCE cookie.
    let login = send_raw(
        &h,
        Request::builder()
            .method("GET")
            .uri("/v1/auth/login?provider=default")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(login.status(), StatusCode::FOUND);
    let plain = set_cookie(&login, "scarab_oauth_state").expect("a state cookie");
    let state = plain.split('.').next().unwrap().to_string();

    // Append the session hash: a plain flow becomes a link flow for `session`.
    let forged = format!("{plain}.{}", api_token_hash(&session.id));
    let cb = send_raw(
        &h,
        Request::builder()
            .method("GET")
            .uri(&format!("/v1/auth/callback?code=good-code&state={state}"))
            .header(
                "cookie",
                format!("scarab_session={}; scarab_oauth_state={forged}", session.id),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    let linked = ids
        .identities_of("amy")
        .await
        .unwrap()
        .into_iter()
        .any(|i| i.provider == "default");
    assert!(
        !linked,
        "a new login identity was attached to amy from a cookie session alone, with no \
         step-up (callback status {})",
        cb.status()
    );
}

/// A harness with a stub login provider `default` (its `/user` is mallory's
/// forge account) and amy signed in through `okta`, in her browser.
async fn link_harness() -> (Harness, Arc<InMemoryIdentities>) {
    let base = stub_provider(json!({ "id": 666, "login": "mallory" })).await;
    let ids = Arc::new(InMemoryIdentities::new());
    ids.sign_in(&identity("okta", "amy-sub", "amy"), "amy", T0)
        .await
        .unwrap();
    let provider = oauth_provider(&base, ids.clone() as Arc<dyn IdentityStore>);
    let wired = ids.clone();
    let h = harness_with(move |st| {
        st.with_oauth_login(provider)
            .with_identities(wired as Arc<dyn IdentityStore>)
    });
    (h, ids)
}

async fn has_default_identity(ids: &InMemoryIdentities) -> bool {
    ids.identities_of("amy")
        .await
        .unwrap()
        .into_iter()
        .any(|i| i.provider == "default")
}

/// The guarded `POST /v1/me/identities/link` under amy's window: its state
/// cookie.
async fn begin_link(h: &Harness, s: &Sess) -> String {
    let resp = send_raw(
        h,
        cookie_req(
            "POST",
            "/v1/me/identities/link",
            Some(json!({"provider": "default"})),
            s,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    set_cookie(&resp, "scarab_oauth_state").expect("a link state cookie")
}

/// `GET /v1/auth/callback` for `cookie` (whose first component is the state)
/// in session `s`.
async fn link_callback(h: &Harness, s: &Sess, cookie: &str) -> axum::response::Response {
    let state = cookie.split('.').next().unwrap();
    send_raw(
        h,
        Request::builder()
            .method("GET")
            .uri(format!("/v1/auth/callback?code=good-code&state={state}"))
            .header(
                "cookie",
                format!("scarab_session={}; scarab_oauth_state={cookie}", s.id),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// The fix must not break linking: amy, stepped up, starts a link through
/// the guarded POST and the callback attaches the account.
#[tokio::test]
async fn a_stepped_up_identity_link_still_completes() {
    let (h, ids) = link_harness().await;
    let (s, mut soft) = amy(&h).await;
    h.open_window(&s, &mut soft).await;

    let cookie = begin_link(&h, &s).await;
    let cb = link_callback(&h, &s, &cookie).await;
    assert_eq!(cb.status(), StatusCode::FOUND);
    assert_eq!(
        cb.headers()[axum::http::header::LOCATION],
        "/account?identity_linked=default"
    );
    assert!(has_default_identity(&ids).await, "the identity was not attached");
}

/// A legitimately minted link component is bound to its flow's `state` and
/// to its session: moved onto a plain-login cookie, or presented from
/// another session of the same human, it is refused and nothing attaches.
#[tokio::test]
async fn a_link_binding_cannot_be_transplanted() {
    let (h, ids) = link_harness().await;
    let (a, mut soft) = amy(&h).await;
    h.open_window(&a, &mut soft).await;
    let minted = begin_link(&h, &a).await;
    let binding = minted.split('.').nth(4).expect("a link component").to_string();
    assert!(!binding.is_empty());

    // (1) Onto a plain login's state, in the same session.
    let login = send_raw(
        &h,
        Request::builder()
            .method("GET")
            .uri("/v1/auth/login?provider=default")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(login.status(), StatusCode::FOUND);
    let plain = set_cookie(&login, "scarab_oauth_state").expect("a state cookie");
    let cb = link_callback(&h, &a, &format!("{plain}.{binding}")).await;
    assert_eq!(cb.status(), StatusCode::UNAUTHORIZED);
    assert!(!has_default_identity(&ids).await, "attached via another state");

    // (2) The genuine cookie, from session B of the same human.
    let b = h.browser_session(owner("amy")).await;
    let cb = link_callback(&h, &b, &minted).await;
    assert_eq!(cb.status(), StatusCode::UNAUTHORIZED);
    assert!(!has_default_identity(&ids).await, "attached via another session");
}

// ---------------------------------------------------------------------------
// 2. A window belongs to a cookie session; Bearer presentation cannot use it
// ---------------------------------------------------------------------------

/// A window is opened in amy's cookie session. Presenting the SAME session id
/// as `Bearer` (as a stolen cookie replayed from a script) for a window crown
/// jewel must not ride that window: a Bearer presentation is never a browser
/// session (ADR-0073 §4, §7).
#[tokio::test]
async fn a_window_is_not_usable_by_bearer_presentation_of_the_cookie_session() {
    let (h, secrets) = with_secrets();
    let (s, mut soft) = amy(&h).await;
    h.open_window(&s, &mut soft).await;

    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret("via-bearer")), &s.id)
        .await;
    assert!(
        !st.is_success(),
        "a window-mode secret write succeeded over a Bearer presentation of a cookie \
         session: {st} {b}"
    );
    assert_eq!(secret_value(&secrets).await, None, "the secret was written");
}

// ---------------------------------------------------------------------------
// 3. Clock boundaries: expiry is exclusive (expires_at is already expired)
// ---------------------------------------------------------------------------

/// A window confirmed at T covers until T+WINDOW exclusive. At exactly
/// `expires_at` it is closed: a window crown jewel is refused.
#[tokio::test]
async fn a_window_is_closed_at_exactly_its_expiry() {
    let h = harness();
    let (s, mut soft) = amy(&h).await;
    // bob exists as a Principal (a binding), so the inside-window write
    // cannot be a vacuous 404.
    h.grant("bob", Scope::Org("acme".into()), Role::Viewer).await;
    h.open_window(&s, &mut soft).await;

    let body = json!({"subject": "bob", "role": "member"});
    // A window crown jewel succeeds inside the window.
    let (st, b) = h
        .cookie("PUT", "/v1/orgs/acme/bindings", Some(body.clone()), &s)
        .await;
    assert!(st.is_success(), "inside window: {st} {b}");

    h.clock.advance(STEP_UP_WINDOW_MS);
    let (st, b) = h
        .cookie("PUT", "/v1/orgs/acme/bindings", Some(body), &s)
        .await;
    assert!(
        is_step_up(st, &b),
        "a window crown jewel was allowed at exactly expires_at: {st} {b}"
    );
}

/// A challenge lives for its TTL exclusive: at exactly `expires_at` it is
/// refused, so a captured-but-stale challenge cannot be answered.
#[tokio::test]
async fn a_challenge_is_refused_at_exactly_its_expiry() {
    let h = harness();
    let (s, mut soft) = amy(&h).await;
    let (st, ch) = h.cookie("POST", "/v1/me/step-up/options", None, &s).await;
    assert_eq!(st, StatusCode::OK, "{ch}");

    h.clock.advance(WEBAUTHN_CHALLENGE_TTL_MS);
    let cred = soft.assert(&ch["public_key"], ORIGIN).unwrap();
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/me/step-up",
            Some(json!({"challenge_id": ch["id"], "credential": cred})),
            &s,
        )
        .await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("challenge_invalid")),
        "a challenge was answerable at exactly its expiry"
    );
}

// ---------------------------------------------------------------------------
// 4. A session created_at=0 (pre-upgrade) is never fresh enough to enroll
// ---------------------------------------------------------------------------

/// A browser-kind session minted before `created_at` was recorded (0) must not
/// count as a fresh login, so it cannot enroll a FIRST passkey — otherwise an
/// old cookie would bootstrap step-up for the attacker.
#[tokio::test]
async fn a_pre_upgrade_session_cannot_enroll_the_first_passkey() {
    let h = harness();
    let sess = Session {
        id: uuid::Uuid::new_v4().to_string(),
        principal: owner("amy"),
        expires_at: h.now().await + DAY,
        csrf: uuid::Uuid::new_v4().to_string(),
        created_at: 0,
        login_kind: LOGIN_KIND_BROWSER.to_string(),
    };
    h.sessions.put(&sess).await.unwrap();
    let s = Sess {
        id: sess.id.clone(),
        csrf: sess.csrf.clone(),
    };
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/me/passkeys/registration/options",
            Some(json!({"label": "x"})),
            &s,
        )
        .await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::FORBIDDEN, json!("fresh_login_required")),
        "a created_at=0 session enrolled a first passkey: {b}"
    );
}

// ---------------------------------------------------------------------------
// 5. An action confirmation is session-scoped
// ---------------------------------------------------------------------------

/// amy has two browser sessions. An always-ask confirmation made in session A
/// must not satisfy the same action in session B: confirmations are keyed by
/// session hash (a stolen cookie in another browser gains nothing).
#[tokio::test]
async fn an_action_confirmation_does_not_cross_sessions() {
    let h = harness();
    let (a, mut soft) = amy(&h).await;
    let b = h.browser_session(owner("amy")).await;
    let uri = "/v1/orgs/acme/tokens";
    let mint = json!({"name": "ci-bot", "role": "member", "expires_in_days": 7});

    // Confirm the action in session A.
    let (st, refusal) = h.cookie("POST", uri, Some(mint.clone()), &a).await;
    assert_step_up(st, &refusal, "action");
    let (st, _) = h.answer(&a, &mut soft, &refusal["challenge"]).await;
    assert_eq!(st, StatusCode::OK);

    // Session B, same action: still refused.
    let (st, body) = h.cookie("POST", uri, Some(mint), &b).await;
    assert!(
        is_step_up(st, &body),
        "an action confirmed in session A satisfied session B: {st} {body}"
    );
}

// ---------------------------------------------------------------------------
// 6. A parked request is visible to its human only
// ---------------------------------------------------------------------------

/// A token owned by amy parks a request. A DIFFERENT admin (bob), with his own
/// passkey, must learn nothing: GET/options/confirm all 404, not even its
/// existence leaks, and he cannot confirm it.
#[tokio::test]
async fn another_admin_cannot_see_or_confirm_a_request() {
    let (h, secrets) = with_secrets();
    let (_amy, _soft) = amy(&h).await;
    let token = pat(&h, "amy", "t-amy", Scope::Org("acme".into()), Role::Admin).await;
    let id = raise(&h, "POST", "/v1/secrets", secret("v"), &token).await;

    h.grant("bob", Scope::Org("acme".into()), Role::Owner).await;
    let bob = h.browser_session(owner("bob")).await;
    let mut bob_soft = passkey();
    h.enroll(&bob, &mut bob_soft, "Bob laptop").await;

    let (st, _) = h
        .cookie("GET", &format!("/v1/step-up/requests/{id}"), None, &bob)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "bob saw the request");

    // He cannot get a challenge for it…
    let (st, b) = h
        .cookie(
            "POST",
            &format!("/v1/step-up/requests/{id}/options"),
            None,
            &bob,
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "bob got a challenge for amy's request: {b}");

    // …nor confirm it with an assertion over a challenge of his own.
    let (st, ch) = h.cookie("POST", "/v1/me/step-up/options", None, &bob).await;
    assert_eq!(st, StatusCode::OK, "{ch}");
    let cred = bob_soft.assert(&ch["public_key"], ORIGIN).unwrap();
    let (st, b) = h
        .cookie(
            "POST",
            &format!("/v1/step-up/requests/{id}/confirm"),
            Some(json!({"challenge_id": ch["id"], "credential": cred})),
            &bob,
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "bob confirmed amy's request: {b}");
    assert_eq!(secret_value(&secrets).await, None, "the secret was written");
}

// ---------------------------------------------------------------------------
// 7. A loosening disguised as a reorder/replace still needs a step-up
// ---------------------------------------------------------------------------

async fn governed_in_memory() -> (Harness, Arc<InMemoryEnvironmentGovernance>) {
    let gov = Arc::new(
        InMemoryEnvironmentGovernance::new().with_environment(
            "acme",
            "prod",
            EnvironmentProjectEligibility::AllProjects,
        ),
    );
    let wired = gov.clone();
    let h = harness_with(move |st| {
        st.with_environment_governance(wired.clone() as Arc<dyn EnvironmentGovernanceStore>)
    });
    (h, gov)
}

/// A protection write that reorders approvers AND replaces one (drops `bob`,
/// adds `carol`) is a loosening (an approver removed), even though the list
/// stays the same length. And `main` → `ma*` widens the allowed refs (the old
/// pattern does not cover the new glob). Both must be refused without a window.
#[tokio::test]
async fn a_reorder_plus_replace_and_a_wider_glob_still_need_a_step_up() {
    let (h, gov) = governed_in_memory().await;
    let (s, _soft) = amy(&h).await;

    gov.put_environment_protection_expecting(
        "acme",
        "prod",
        &EnvironmentProtectionRules {
            approvers: vec!["amy".into(), "bob".into()],
            allowed_refs: vec!["main".into()],
            ..Default::default()
        },
        None,
        "seed",
        0,
    )
    .await
    .unwrap();

    let uri = "/v1/orgs/acme/environments/prod/protection";

    // Reorder + replace: bob dropped, carol added. Same length, still loosens.
    let (st, b) = h
        .cookie(
            "PUT",
            uri,
            Some(json!({"approvers": ["carol", "amy"], "allowed_refs": ["main"]})),
            &s,
        )
        .await;
    assert!(
        is_step_up(st, &b),
        "dropping an approver under a reorder was allowed without a step-up: {st} {b}"
    );

    // `main` -> `ma*` widens (the old pattern does not subsume the new glob).
    let (st, b) = h
        .cookie(
            "PUT",
            uri,
            Some(json!({"approvers": ["amy", "bob"], "allowed_refs": ["ma*"]})),
            &s,
        )
        .await;
    assert!(
        is_step_up(st, &b),
        "widening an allowed ref to `ma*` was allowed without a step-up: {st} {b}"
    );
}

// ---------------------------------------------------------------------------
// 8. The pending-request cap is per credential (documented behaviour)
// ---------------------------------------------------------------------------

/// The 10-request cap is per requester CREDENTIAL. Two tokens of one human can
/// each hold 10, so one human can accumulate 20 pending confirmations. This
/// documents that behaviour; the invariant that still holds is that NONE of
/// them runs until the human confirms each in a browser.
#[tokio::test]
async fn the_pending_cap_is_per_credential_not_per_human() {
    let (h, secrets) = with_secrets();
    let (_amy, _soft) = amy(&h).await;
    let t1 = pat(&h, "amy", "t1", Scope::Org("acme".into()), Role::Admin).await;
    let t2 = pat(&h, "amy", "t2", Scope::Org("acme".into()), Role::Admin).await;
    for i in 0..10 {
        raise(&h, "POST", "/v1/secrets", secret(&format!("a{i}")), &t1).await;
        raise(&h, "POST", "/v1/secrets", secret(&format!("b{i}")), &t2).await;
    }
    // 20 pending for one human — the cap did not stop the second credential.
    assert_eq!(h.step_up.requests().len(), 20);
    // …but nothing ran.
    assert_eq!(secret_value(&secrets).await, None, "a parked request ran");
}
