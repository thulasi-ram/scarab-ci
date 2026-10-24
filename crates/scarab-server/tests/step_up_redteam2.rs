//! RED TEAM round 2 — ADR-0073 step-up review (2026-10-24), aimed at the
//! round-1 FIXES (dd75e5d..43ece17).
//!
//! Each test is ONE attack and asserts the SECURE outcome: a FAILING test
//! means the attack worked. The software passkey and a stub OAuth provider
//! are the only fakes of true externals; everything else is the real router
//! over the in-memory stores (hermetic: no Postgres needed).

mod step_up_harness;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{json, Value};

use scarab_identity::{
    api_token_hash, ApiToken, ApiTokenStore, IdentityStore, ProviderIdentity, Role, Scope,
};
use scarab_project::{
    EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProtectionRules,
};
use scarab_secrets::{SecretProvider, SecretScope};
use scarab_server::config::{OAuthConfig, StepUpMode};
use scarab_server::oauth::OAuthAuthenticator;
use scarab_server::step_up::check_step_up_composition;
use scarab_server::step_up::ops::{escape_invisible, OpInput, PLUGIN_CONNECTION_PUT, SECRET_PUT};
use scarab_testkit::{FakeSecrets, InMemoryEnvironmentGovernance, InMemoryIdentities};
use scarab_webauthn::soft::SoftPasskey;

use step_up_harness::*;

const MIN: i64 = 60 * 1000;
const HOUR: i64 = 60 * MIN;
const DAY: i64 = 24 * HOUR;

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

/// amy: Owner of `acme`, in her browser, with a passkey.
async fn amy(h: &Harness) -> (Sess, SoftPasskey) {
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (s, soft)
}

/// `subject` with `role` in `org` only (no global role), in a browser, with
/// a passkey.
async fn member(h: &Harness, subject: &str, org: &str, role: Role) -> (Sess, SoftPasskey) {
    h.grant(subject, Scope::Org(org.into()), role).await;
    let s = h.browser_session(scoped(subject)).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, &format!("{subject}'s key")).await;
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

/// A cookie request whose JSON body is sent verbatim (the harness only
/// takes a `Value`, which cannot hold `1e400` or 300 levels of nesting).
fn raw_cookie_req(method: &str, uri: &str, body: &str, s: &Sess) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("scarab_session={}", s.id))
        .header("x-csrf-token", &s.csrf)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn raw_bearer_req(method: &str, uri: &str, body: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Request bodies serde_json's `Value` parser REFUSES but a typed
/// `Json<T>` extractor ACCEPTS (unknown fields are skipped by serde_json's
/// iterative `ignore_value`, which has no recursion limit and does not range
/// check numbers).
fn junk_secret_bodies() -> Vec<(&'static str, String)> {
    let base = r#""org":"acme","name":"DB_PASSWORD","value":"exfil""#;
    let deep = format!("{}{}", "[".repeat(300), "]".repeat(300));
    vec![
        ("300-deep array", format!("{{{base},\"junk\":{deep}}}")),
        ("1e400 number", format!("{{{base},\"junk\":1e400}}")),
    ]
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

fn oauth_provider_named(
    base: &str,
    ids: Arc<dyn IdentityStore>,
    id: &str,
    unqualified: bool,
    owners: &[&str],
) -> Arc<OAuthAuthenticator> {
    Arc::new(
        OAuthAuthenticator::new(OAuthConfig {
            client_id: "cid".into(),
            client_secret: "sekret".into(),
            authorize_url: format!("{base}/authorize"),
            token_url: format!("{base}/token"),
            userinfo_url: format!("{base}/user"),
            oidc_issuer: None,
            scopes: String::new(),
            owners: owners.iter().map(|o| o.to_string()).collect(),
            provider_id: id.into(),
            unqualified_ids: unqualified,
            name: id.into(),
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

/// A harness with stub login provider `default` (its `/user` is mallory's
/// forge account) — plus `extra` providers — and amy signed in through
/// `okta`, in her browser.
async fn link_harness_with(extra: &[&str]) -> (Harness, Arc<InMemoryIdentities>) {
    let base = stub_provider(json!({ "id": 666, "login": "mallory" })).await;
    let ids = Arc::new(InMemoryIdentities::new());
    ids.sign_in(&identity("okta", "amy-sub", "amy"), "amy", T0)
        .await
        .unwrap();
    let mut providers = vec![oauth_provider_named(
        &base,
        ids.clone() as Arc<dyn IdentityStore>,
        "default",
        true,
        &[],
    )];
    for id in extra {
        providers.push(oauth_provider_named(
            &base,
            ids.clone() as Arc<dyn IdentityStore>,
            id,
            false,
            &[],
        ));
    }
    let wired = ids.clone();
    let h = harness_with(move |st| {
        let mut st = st.with_identities(wired as Arc<dyn IdentityStore>);
        for p in providers {
            st = st.with_oauth_login(p);
        }
        st
    });
    (h, ids)
}

async fn amy_identities(ids: &InMemoryIdentities) -> Vec<String> {
    ids.identities_of("amy")
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.provider)
        .collect()
}

/// The guarded `POST /v1/me/identities/link` (amy must hold a window): its
/// state cookie.
async fn begin_link(h: &Harness, s: &Sess, provider: &str) -> String {
    let resp = send_raw(
        h,
        cookie_req(
            "POST",
            "/v1/me/identities/link",
            Some(json!({ "provider": provider })),
            s,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    set_cookie(&resp, "scarab_oauth_state").expect("a link state cookie")
}

/// `GET /v1/auth/callback` for `cookie` (first component = state) in `s`.
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

/// Replace component `i` of a `.`-joined state cookie.
fn with_component(cookie: &str, i: usize, value: &str) -> String {
    let mut parts: Vec<String> = cookie.split('.').map(str::to_string).collect();
    parts[i] = value.to_string();
    parts.join(".")
}

// ---------------------------------------------------------------------------
// FIX #1 — the identity-link HMAC binding
// ---------------------------------------------------------------------------

/// The binding is HMAC(K_link, state, session) and nothing else: it carries
/// no time. amy steps up, starts a link, and abandons it. Hours later — her
/// window long closed, the cookie's 10-minute Max-Age long past (a browser
/// limit, not a server one) — a holder of her cookie jar replays the state
/// cookie with their own forge login. SECURE: the stepped-up authority of a
/// link flow ends with the window (or the flow's TTL); nothing attaches.
#[tokio::test]
async fn a_link_binding_does_not_outlive_its_step_up() {
    let (h, ids) = link_harness_with(&[]).await;
    let (s, mut soft) = amy(&h).await;
    h.open_window(&s, &mut soft).await;
    let cookie = begin_link(&h, &s, "default").await;

    // Abandoned. Two hours later: window (5 min) and cookie Max-Age (10 min)
    // both long gone; amy's session (24 h) is still live.
    h.clock.advance(2 * HOUR);
    let cb = link_callback(&h, &s, &cookie).await;
    assert!(
        !amy_identities(&ids).await.contains(&"default".to_string()),
        "a link flow minted under a window completed 2 h later with no step-up \
         (callback {}): the binding has no lifetime",
        cb.status()
    );
}

/// The binding covers `state` and the session but NOT the provider. amy
/// confirmed linking a `default` account; the `provider` component of the
/// (unsigned) state cookie is rewritten to `other`. SECURE: the link lands
/// on the provider the guarded POST was asked for, or not at all.
#[tokio::test]
async fn a_link_binding_cannot_be_moved_to_another_provider() {
    let (h, ids) = link_harness_with(&["other"]).await;
    let (s, mut soft) = amy(&h).await;
    h.open_window(&s, &mut soft).await;
    let cookie = begin_link(&h, &s, "default").await;
    assert_eq!(cookie.split('.').nth(3), Some("default"), "{cookie}");

    let swapped = with_component(&cookie, 3, "other");
    let cb = link_callback(&h, &s, &swapped).await;
    assert!(
        !amy_identities(&ids).await.contains(&"other".to_string()),
        "a link confirmed for provider `default` attached an `other` account \
         (callback {} {:?})",
        cb.status(),
        cb.headers().get(axum::http::header::LOCATION)
    );
}

/// Encodings of a genuine binding: uppercase hex, one hex digit short, one
/// extra, all zeros, and a different session's legacy bare session hash.
/// SECURE: every variant is 401 and nothing attaches (ct_eq over the exact
/// lowercase hex). Last: an EMPTY link component makes it a plain login —
/// which signs the forge account in as itself, never onto amy.
#[tokio::test]
async fn a_link_binding_in_another_encoding_is_refused() {
    let (h, ids) = link_harness_with(&[]).await;
    let (s, mut soft) = amy(&h).await;
    h.open_window(&s, &mut soft).await;
    let cookie = begin_link(&h, &s, "default").await;
    let link = cookie
        .split('.')
        .nth(4)
        .expect("a link component")
        .to_string();
    assert_eq!(link.len(), 64, "{link}");

    let variants = [
        ("uppercase", link.to_uppercase()),
        ("truncated", link[..63].to_string()),
        ("extended", format!("{link}0")),
        ("zeros", "0".repeat(64)),
        ("bare session hash", api_token_hash(&s.id)),
    ];
    for (what, v) in variants {
        let cb = link_callback(&h, &s, &with_component(&cookie, 4, &v)).await;
        assert_eq!(cb.status(), StatusCode::UNAUTHORIZED, "{what}");
        assert!(amy_identities(&ids).await.len() == 1, "{what}: attached");
    }

    // Empty link component: a plain login, not a link of amy's account.
    let plain: Vec<&str> = cookie.split('.').take(4).collect();
    let _ = link_callback(&h, &s, &plain.join(".")).await;
    assert!(
        !amy_identities(&ids).await.contains(&"default".to_string()),
        "an empty link component attached the account to amy"
    );
}

// ---------------------------------------------------------------------------
// FIX #2 — Org scoping of confirmations
// ---------------------------------------------------------------------------

/// Fix #2 writes `step_up.confirmed` under the Org the GUARD resolved — from
/// the request path — before any handler authorization. mallory (Owner of
/// `evil` only) asks for a token in `acme`, where she holds nothing, and
/// answers the challenge with her own passkey. SECURE: no confirmation by a
/// non-member, with her chosen summary text, lands in acme's security log.
#[tokio::test]
async fn a_foreign_admin_cannot_plant_a_confirmation_in_another_orgs_log() {
    let h = harness();
    let (amy, _) = amy(&h).await;
    let (mallory, mut soft) = member(&h, "mallory", "evil", Role::Owner).await;

    let mint = json!({
        "name": "approved-by-acme-security-team",
        "role": "member",
        "expires_in_days": 7
    });
    let (st, refusal) = h
        .cookie("POST", "/v1/orgs/acme/tokens", Some(mint), &mallory)
        .await;
    if is_step_up(st, &refusal) {
        let (st, conf) = h.answer(&mallory, &mut soft, &refusal["challenge"]).await;
        assert_eq!(st, StatusCode::OK, "{conf}");
    }

    let (st, page) = h
        .cookie("GET", "/v1/orgs/acme/security/events", None, &amy)
        .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    let planted: Vec<&Value> = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "step_up.confirmed" && e["subject"] == "mallory")
        .collect();
    assert!(
        planted.is_empty(),
        "a non-member's step_up.confirmed (with her summary) is in acme's log: {planted:?}"
    );
}

/// An Org-less confirmation (identity.link, OrgOf::None) by amy — Admin of
/// `big`, only a Viewer of `tiny` — seen by tom, Admin of `tiny`. SECURE:
/// tom's view of tiny's log never says what it was for (op, target, summary)
/// and carries no Org-less op.authorised at all.
#[tokio::test]
async fn an_orgless_confirmation_never_says_what_it_was_for_in_another_org() {
    let (h, _ids) = link_harness_with(&[]).await;
    h.grant("amy", Scope::Org("tiny".into()), Role::Viewer)
        .await;
    let (amy, mut soft) = member(&h, "amy", "big", Role::Admin).await;
    let (tom, _) = member(&h, "tom", "tiny", Role::Admin).await;

    let (st, refusal) = h
        .cookie(
            "POST",
            "/v1/me/identities/link",
            Some(json!({"provider": "default"})),
            &amy,
        )
        .await;
    assert_step_up(st, &refusal, "window");
    let (st, conf) = h.answer(&amy, &mut soft, &refusal["challenge"]).await;
    assert_eq!(st, StatusCode::OK, "{conf}");
    // Now authorised: writes an Org-less op.authorised.
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/me/identities/link",
            Some(json!({"provider": "default"})),
            &amy,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");

    let (st, page) = h
        .cookie("GET", "/v1/orgs/tiny/security/events", None, &tom)
        .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    let text = page.to_string();
    assert!(
        !text.contains("identity.link") && !text.contains("op.authorised"),
        "tiny's admin learns what amy confirmed elsewhere: {page}"
    );
}

// ---------------------------------------------------------------------------
// FIX #5 (+#2) — the guard parses the body differently from the handler
// ---------------------------------------------------------------------------

/// The guard reads the body with `serde_json::from_slice::<Value>`; on error
/// it treats the body as opaque: no target, no changes, and — for
/// `OrgOf::Body` ops — NO Org. The handler's `Json<PutSecretRequest>` skips
/// unknown fields with an iterative, unlimited `ignore_value`, so a junk
/// field (300-deep nesting, or `1e400`) parses there but not in the guard.
/// amy (Owner, in her window) writes acme's DB_PASSWORD with such a body.
/// SECURE: either the write is refused, or its `op.authorised` names Org
/// `acme` and the secret — acme's security log must not lose a secret write.
#[tokio::test]
async fn an_unparseable_body_cannot_hide_a_secret_write_from_its_org_log() {
    for (what, body) in junk_secret_bodies() {
        let (h, secrets) = with_secrets();
        let (s, mut soft) = amy(&h).await;
        h.open_window(&s, &mut soft).await;

        let resp = send_raw(&h, raw_cookie_req("POST", "/v1/secrets", &body, &s)).await;
        let status = resp.status();
        if !status.is_success() {
            continue; // refused: the attack failed for this body
        }
        assert_eq!(secret_value(&secrets).await.as_deref(), Some("exfil"));
        let authorised = h.events("op.authorised").await;
        let ev = authorised
            .iter()
            .find(|e| e.op.as_deref() == Some("secret.put"))
            .expect("an op.authorised for the write");
        assert!(
            ev.org.as_deref() == Some("acme")
                && ev
                    .target
                    .as_deref()
                    .is_some_and(|t| t.contains("DB_PASSWORD")),
            "{what}: a secret write ran ({status}) but its op.authorised has org {:?}, \
             target {:?} — invisible in acme's Org log",
            ev.org,
            ev.target
        );
    }
}

/// The same parser split, on a token-raised StepUpConfirmationRequest: the
/// human is shown the summary the guard rendered. SECURE: a request that
/// writes acme's DB_PASSWORD never runs after being shown to its human with
/// a summary that does not name the secret.
#[tokio::test]
async fn an_unparseable_body_cannot_blank_what_the_human_confirms() {
    for (what, body) in junk_secret_bodies() {
        let (h, secrets) = with_secrets();
        let (amy, mut soft) = amy(&h).await;
        let token = pat(&h, "amy", "t1", Scope::Org("acme".into()), Role::Admin).await;
        let resp = send_raw(&h, raw_bearer_req("POST", "/v1/secrets", &body, &token)).await;
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let b: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if !is_step_up(status, &b) {
            continue; // no request raised
        }
        let id = b["request_id"].as_str().expect("a request").to_string();
        let (st, shown) = h
            .cookie("GET", &format!("/v1/step-up/requests/{id}"), None, &amy)
            .await;
        assert_eq!(st, StatusCode::OK, "{shown}");
        let target = shown["summary"]["target"].as_str().unwrap_or_default();

        // amy confirms what she was shown.
        let (st, ch) = h
            .cookie(
                "POST",
                &format!("/v1/step-up/requests/{id}/options"),
                None,
                &amy,
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{ch}");
        let cred = soft.assert(&ch["public_key"], ORIGIN).unwrap();
        let (st, done) = h
            .cookie(
                "POST",
                &format!("/v1/step-up/requests/{id}/confirm"),
                Some(json!({"challenge_id": ch["id"], "credential": cred})),
                &amy,
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{done}");

        let written = secret_value(&secrets).await.is_some();
        assert!(
            !(written && !target.contains("DB_PASSWORD")),
            "{what}: the human confirmed target {target:?}, changes {} (org {}) — \
             and acme/DB_PASSWORD was written",
            shown["summary"]["changes"],
            shown["org"]
        );
    }
}

/// `?subject=bob&subject=carol` on a binding revoke: the guard's summary and
/// event take the FIRST value (`query_value` = find). SECURE: the handler
/// either refuses the duplicate or acts on the subject the event names.
#[tokio::test]
async fn a_repeated_query_parameter_cannot_split_the_event_from_the_action() {
    let h = harness();
    let (s, mut soft) = amy(&h).await;
    h.grant("bob", Scope::Org("acme".into()), Role::Viewer)
        .await;
    h.grant("carol", Scope::Org("acme".into()), Role::Viewer)
        .await;
    h.open_window(&s, &mut soft).await;

    let (st, b) = h
        .cookie(
            "DELETE",
            "/v1/orgs/acme/bindings?subject=bob&subject=carol",
            None,
            &s,
        )
        .await;
    let carol_gone = !h
        .rbac_store
        .orgs_of("carol")
        .await
        .unwrap()
        .contains(&"acme".to_string());
    if carol_gone {
        let named_carol = h
            .events("op.authorised")
            .await
            .iter()
            .any(|e| e.target.as_deref().is_some_and(|t| t.contains("carol")));
        assert!(
            named_carol,
            "carol's binding was revoked ({st} {b}) under an op.authorised that names bob"
        );
    }
}

// ---------------------------------------------------------------------------
// FIX #5 — summary rendering (pure: `describe_generic`, `escape_invisible`)
// ---------------------------------------------------------------------------

fn plugin_connection_input(base_url: &str) -> OpInput {
    OpInput {
        method: "PUT".into(),
        path: "/v1/orgs/acme/plugins/argocd/connections/api".into(),
        params: vec![
            ("org".into(), "acme".into()),
            ("name".into(), "argocd".into()),
            ("slot".into(), "api".into()),
        ],
        query: vec![],
        body: json!({"base_url": base_url, "credential": "(hidden)"}),
    }
}

/// "A URL is never cut, and gets a host line" keys on
/// `starts_with("http://" | "https://")`. The handler
/// (`normalize_connection_base_url`) `trim()`s and parses with the WHATWG
/// parser, which also accepts an uppercase scheme. A padded host — wildcard
/// DNS under evil.example — plus a long path puts the real domain in the
/// elided middle. SECURE: the summary of a plugin connection names the
/// destination's registrable domain whichever spelling reaches the handler.
#[tokio::test]
async fn a_plugin_destination_is_visible_however_its_url_is_spelled() {
    let host = format!(
        "good.example.com.{}.{}.{}.evil.example",
        "a".repeat(60),
        "b".repeat(60),
        "c".repeat(60)
    );
    let path = "p".repeat(120);
    let shows_evil = |url: &str| {
        let s = PLUGIN_CONNECTION_PUT.summary(&plugin_connection_input(url));
        s.changes.iter().any(|c| c.contains("evil.example"))
    };
    // Control: the canonical spelling is shown in full (fix #5 works).
    assert!(shows_evil(&format!("https://{host}/{path}")), "control");
    for url in [
        format!(" https://{host}/{path}"),
        format!("HTTPS://{host}/{path}"),
    ] {
        assert!(
            shows_evil(&url),
            "the summary hides the destination's domain for base_url {:?}: {:?}",
            &url[..12],
            PLUGIN_CONNECTION_PUT
                .summary(&plugin_connection_input(&url))
                .changes
        );
    }
}

/// Fix #5 escapes C0/C1 and a hand-listed set of invisibles. Missing: U+061C
/// ARABIC LETTER MARK (a Bidi_Control, like U+200F), U+2028/U+2029
/// (line/paragraph separators), U+00AD soft hyphen, U+180E, and the
/// invisible TAG characters U+E0020–U+E007F. SECURE: every bidi control and
/// default-ignorable character reaches the human escaped.
#[tokio::test]
async fn every_bidi_control_and_invisible_character_is_escaped() {
    let missing: Vec<String> = [
        '\u{061C}',
        '\u{2028}',
        '\u{2029}',
        '\u{00AD}',
        '\u{180E}',
        '\u{E0041}',
    ]
    .iter()
    .filter(|c| escape_invisible(&format!("DB{c}PASSWORD")).contains(**c))
    .map(|c| format!("U+{:04X}", *c as u32))
    .collect();
    assert!(missing.is_empty(), "rendered raw: {missing:?}");
}

/// Escaping is not injective: a backslash is not escaped, so a name typed as
/// the literal text `\u{202E}` renders exactly like a real U+202E. SECURE:
/// two different values never render the same.
#[tokio::test]
async fn an_escaped_character_and_its_literal_spelling_render_differently() {
    assert_ne!(
        escape_invisible("prod\u{202E}db"),
        escape_invisible("prod\\u{202E}db"),
        "a literal `\\u{{202E}}` is indistinguishable from the real character"
    );
}

/// The host line of an IDN look-alike (`аcme.example`, Cyrillic а) reads
/// exactly like the ASCII host. SECURE: a non-ASCII host is shown as
/// punycode, or flagged.
#[tokio::test]
async fn a_confusable_host_is_not_shown_as_its_lookalike() {
    let s = PLUGIN_CONNECTION_PUT.summary(&plugin_connection_input(
        "https://\u{0430}cme.example/hooks",
    ));
    let host_line = s
        .changes
        .iter()
        .find(|c| c.starts_with("base_url host"))
        .expect("a host line");
    assert!(
        host_line.is_ascii(),
        "the host line shows a mixed-script look-alike as-is: {host_line}"
    );
}

/// Flattening joins nested keys with `.`, so `{"a.b": X}` and
/// `{"a": {"b": Y}}` both render `a.b = …`. SECURE: two different leaves
/// never render under the same path.
#[tokio::test]
async fn two_body_leaves_never_render_under_one_path() {
    let input = OpInput {
        method: "POST".into(),
        path: "/v1/secrets".into(),
        params: vec![],
        query: vec![],
        body: json!({
            "org": "acme", "name": "DB_PASSWORD", "value": "(hidden)",
            "repo.name": "decoy", "repo": {"name": "real"}
        }),
    };
    let s = SECRET_PUT.summary(&input);
    let n = s
        .changes
        .iter()
        .filter(|c| c.starts_with("repo.name = "))
        .count();
    assert!(n <= 1, "ambiguous lines: {:?}", s.changes);
}

// ---------------------------------------------------------------------------
// FIX #4 — replay pinning by rendered change strings
// ---------------------------------------------------------------------------

async fn governed() -> (Harness, Arc<InMemoryEnvironmentGovernance>) {
    let gov = Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
        "acme",
        "prod",
        EnvironmentProjectEligibility::AllProjects,
    ));
    let wired = gov.clone();
    let h = harness_with(move |st| {
        st.with_environment_governance(wired as Arc<dyn EnvironmentGovernanceStore>)
    });
    (h, gov)
}

async fn protection(gov: &InMemoryEnvironmentGovernance) -> EnvironmentProtectionRules {
    gov.deploy_governance("acme", "", "prod")
        .await
        .unwrap()
        .expect("acme/prod")
        .environment
        .protection
}

/// The human confirms "removes approver `bob`". Before the confirm, a
/// concurrent admin removes bob AND adds carol, so the same body now removes
/// carol instead — same count, same shape, a different person. SECURE: the
/// replay refuses (the request fails) and carol stays an approver.
#[tokio::test]
async fn a_replay_cannot_swap_which_approver_it_removes() {
    let (h, gov) = governed().await;
    let (amy, mut soft) = amy(&h).await;
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
    let token = pat(&h, "amy", "t1", Scope::Org("acme".into()), Role::Admin).await;
    let (st, b) = h
        .bearer(
            "PUT",
            "/v1/orgs/acme/environments/prod/protection",
            Some(json!({"approvers": ["amy"], "allowed_refs": ["main"]})),
            &token,
        )
        .await;
    assert!(is_step_up(st, &b), "{st} {b}");
    assert_eq!(
        b["summary"]["changes"],
        json!(["removes approver `bob`"]),
        "{b}"
    );
    let id = b["request_id"].as_str().expect("a request").to_string();

    let current = protection(&gov).await;
    gov.put_environment_protection_expecting(
        "acme",
        "prod",
        &EnvironmentProtectionRules {
            approvers: vec!["amy".into(), "carol".into()],
            allowed_refs: vec!["main".into()],
            ..Default::default()
        },
        Some(&current),
        "bob",
        1,
    )
    .await
    .unwrap();

    let (st, ch) = h
        .cookie(
            "POST",
            &format!("/v1/step-up/requests/{id}/options"),
            None,
            &amy,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{ch}");
    let cred = soft.assert(&ch["public_key"], ORIGIN).unwrap();
    let (st, done) = h
        .cookie(
            "POST",
            &format!("/v1/step-up/requests/{id}/confirm"),
            Some(json!({"challenge_id": ch["id"], "credential": cred})),
            &amy,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_ne!(done["state"], "executed", "{done}");
    assert!(
        protection(&gov)
            .await
            .approvers
            .contains(&"carol".to_string()),
        "carol was removed by a replay its human confirmed as `removes approver bob`"
    );
}

// ---------------------------------------------------------------------------
// FIX #6 — fail-closed events leave no half-state
// ---------------------------------------------------------------------------

/// The request confirm's step_up.confirmed fails to append, so the request
/// settles `failed` with nothing run. The log recovers. SECURE: the same
/// request cannot be confirmed again into running (no half-state where it is
/// still pending/executing and replayable).
#[tokio::test]
async fn an_unlogged_request_confirmation_cannot_be_retried_into_running() {
    let (h, secrets) = with_secrets();
    let (amy, mut soft) = amy(&h).await;
    let token = pat(&h, "amy", "t1", Scope::Org("acme".into()), Role::Admin).await;
    let (st, b) = h
        .bearer(
            "POST",
            "/v1/secrets",
            Some(json!({"org": "acme", "name": "DB_PASSWORD", "value": "v"})),
            &token,
        )
        .await;
    assert!(is_step_up(st, &b), "{st} {b}");
    let id = b["request_id"].as_str().expect("a request").to_string();
    let options = format!("/v1/step-up/requests/{id}/options");
    let confirm = format!("/v1/step-up/requests/{id}/confirm");

    let (st, ch) = h.cookie("POST", &options, None, &amy).await;
    assert_eq!(st, StatusCode::OK, "{ch}");
    let cred = soft.assert(&ch["public_key"], ORIGIN).unwrap();
    h.events.fail_appends(true);
    let (_, first) = h
        .cookie(
            "POST",
            &confirm,
            Some(json!({"challenge_id": ch["id"], "credential": cred})),
            &amy,
        )
        .await;
    h.events.fail_appends(false);
    assert_ne!(first["state"], "executed", "{first}");

    let (st, ch) = h.cookie("POST", &options, None, &amy).await;
    if st == StatusCode::OK {
        let cred = soft.assert(&ch["public_key"], ORIGIN).unwrap();
        let (_, again) = h
            .cookie(
                "POST",
                &confirm,
                Some(json!({"challenge_id": ch["id"], "credential": cred})),
                &amy,
            )
            .await;
        assert_ne!(again["state"], "executed", "{again}");
    }
    assert_eq!(secret_value(&secrets).await, None, "the secret was written");
}

// ---------------------------------------------------------------------------
// FIX #3 — install-owner reset
// ---------------------------------------------------------------------------

/// The owners entry is provider-qualified and UPPERCASE
/// (`default:ROOT@ACME.TEST`); root's linked identity carries the verified
/// email in lowercase. SECURE: still an install owner — an Org Owner cannot
/// strip root's passkeys.
#[tokio::test]
async fn an_install_owner_matched_by_email_case_is_still_protected() {
    let ids = Arc::new(InMemoryIdentities::new());
    ids.sign_in(
        &ProviderIdentity {
            provider: "default".into(),
            external_id: "root-sub".into(),
            handle: "root".into(),
            email: Some("root@acme.test".into()),
            email_verified: true,
            display_name: None,
        },
        "root",
        T0,
    )
    .await
    .unwrap();
    let wired = ids.clone();
    let provider = oauth_provider_named(
        "http://127.0.0.1:9",
        ids.clone() as Arc<dyn IdentityStore>,
        "default",
        true,
        &["default:ROOT@ACME.TEST"],
    );
    let h = harness_with(move |st| {
        st.with_oauth_login(provider)
            .with_identities(wired as Arc<dyn IdentityStore>)
    });
    let (mallory, mut soft) = member(&h, "mallory", "evil", Role::Owner).await;
    let _root = member(&h, "root", "evil", Role::Viewer).await;
    assert_eq!(h.passkey_store.list("root").await.unwrap().len(), 1);

    let uri = "/v1/orgs/evil/members/root/passkeys/reset";
    let (st, b) = h.cookie("POST", uri, None, &mallory).await;
    if is_step_up(st, &b) {
        let (st, conf) = h.answer(&mallory, &mut soft, &b["challenge"]).await;
        assert_eq!(st, StatusCode::OK, "{conf}");
        let (st, b) = h.cookie("POST", uri, None, &mallory).await;
        assert!(!st.is_success(), "reset succeeded: {st} {b}");
    }
    assert_eq!(
        h.passkey_store.list("root").await.unwrap().len(),
        1,
        "an Org Owner stripped an email-matched install owner's passkeys"
    );
}

// ---------------------------------------------------------------------------
// FIX #7 — boot refusal
// ---------------------------------------------------------------------------

/// `SCARAB_STEP_UP` spellings that must refuse the boot outside the dev
/// escape hatch (a look-alike of `enforce` must not quietly mean "off"),
/// and the composition check over every login-without-step-up combination.
#[tokio::test]
async fn no_spelling_or_composition_boots_login_without_step_up() {
    for ok in [
        None,
        Some(""),
        Some("  "),
        Some("enforce"),
        Some(" ENFORCE "),
    ] {
        assert_eq!(
            StepUpMode::resolve(ok, false).unwrap(),
            StepUpMode::Enforce,
            "{ok:?}"
        );
    }
    for bad in [
        "off",
        "OFF",
        "log",
        "0",
        "false",
        "disabled",
        "enforce\u{200B}",
        "enforced",
        "en force",
        "enforce,off",
    ] {
        assert!(
            StepUpMode::resolve(Some(bad), false).is_err(),
            "`{bad:?}` booted outside SCARAB_DEV_INSECURE"
        );
    }
    // Login wired, not enforced, not dev: refused. Everything else boots.
    assert!(check_step_up_composition(false, true, false).is_err());
    assert!(check_step_up_composition(false, true, true).is_ok());
}

// ---------------------------------------------------------------------------
// FIX #10 — the UI cannot be framed
// ---------------------------------------------------------------------------

/// Every UI answer — a real dist file other than index.html, the SPA
/// fallback under an unknown /v1 path, a traversal attempt, and a non-GET —
/// carries both anti-framing headers. SECURE: none is frameable.
#[tokio::test]
async fn no_ui_answer_is_frameable() {
    use tower::ServiceExt;
    let dist = tempfile::tempdir().unwrap();
    std::fs::write(dist.path().join("index.html"), "<html>scarab-ui</html>").unwrap();
    std::fs::write(dist.path().join("other.html"), "<html>other</html>").unwrap();
    let db = Arc::new(scarab_testkit::InMemoryDb::new());
    let store = Arc::new(scarab_testkit::InMemoryObjectStore::new());
    let logs = Arc::new(scarab_server::LogService::new(store, db.clone()));
    let app = scarab_server::router(
        scarab_server::AppState::new(db, Arc::new(scarab_testkit::FakeClock::new(0)), logs)
            .with_ui_dir(dist.path()),
    );
    for (method, uri) in [
        ("GET", "/other.html"),
        ("GET", "/v1/does-not-exist"),
        ("GET", "/assets/../index.html"),
        ("POST", "/security/confirm/x"),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if resp.status() == StatusCode::OK {
            let h = |n: &str| {
                resp.headers()
                    .get(n)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            assert_eq!(
                h("content-security-policy").as_deref(),
                Some("frame-ancestors 'none'"),
                "{method} {uri}"
            );
            assert_eq!(
                h("x-frame-options").as_deref(),
                Some("DENY"),
                "{method} {uri}"
            );
        }
    }
}
