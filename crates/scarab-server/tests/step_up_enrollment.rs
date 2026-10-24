//! ADR-0073 C2: enrollment bootstrap. The FIRST passkey needs this browser
//! session's login to have completed within 5 minutes (else the answer says
//! to log in again); later adds and removals are always-ask crown jewels;
//! enrollment is prompted, never forced; and a login is never a step-up.

mod step_up_harness;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use serde_json::json;
use tower::ServiceExt;

use scarab_identity::LOGIN_KIND_API;

use step_up_harness::*;

const MIN: i64 = 60 * 1000;

fn cookie(headers: &axum::http::HeaderMap, name: &str) -> String {
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| c.strip_prefix(&format!("{name}=")))
        .map(|c| c.split(';').next().unwrap().to_string())
        .unwrap_or_else(|| panic!("no {name} cookie"))
}

/// Sign in through the browser OAuth callback (the FakeAuthenticator is the
/// provider), landing on `next`. Returns the new session and the Location.
async fn browser_login(h: &Harness, code: &str, next: &str) -> (Sess, String) {
    let next_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(next);
    let flow_cookie = format!("st8te.....{next_b64}");
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/auth/callback?code={code}&state=st8te"))
                .header("cookie", format!("scarab_oauth_state={flow_cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    let location = resp.headers()["location"].to_str().unwrap().to_string();
    let s = Sess {
        id: cookie(resp.headers(), "scarab_session"),
        csrf: cookie(resp.headers(), "scarab_csrf"),
    };
    (s, location)
}

async fn registration_options(h: &Harness, s: &Sess) -> (StatusCode, serde_json::Value) {
    h.cookie(
        "POST",
        "/v1/me/passkeys/registration/options",
        Some(json!({"label": "Laptop"})),
        s,
    )
    .await
}

/// A fresh browser login may enroll the first passkey for 5 minutes (and the
/// callback lands on the `next` it was started with); after that the answer
/// is `fresh_login_required`, pointing at a fresh login.
#[tokio::test]
async fn the_first_passkey_needs_a_fresh_browser_login() {
    let h = harness();
    let (s, location) = browser_login(&h, "amy-code", "/account/security?enroll=1").await;
    assert_eq!(location, "/account/security?enroll=1");

    let (st, b) = registration_options(&h, &s).await;
    assert_eq!(st, StatusCode::OK, "+0: {b}");
    h.clock.advance(4 * MIN + 59 * 1000);
    let (st, b) = registration_options(&h, &s).await;
    assert_eq!(st, StatusCode::OK, "+4:59: {b}");
    assert_eq!(b["public_key"]["rp"]["id"], "ci.example.com");
    assert_eq!(
        b["public_key"]["authenticatorSelection"]["userVerification"],
        "required"
    );

    h.clock.advance(2 * 1000);
    let (st, b) = registration_options(&h, &s).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "+5:01: {b}");
    assert_eq!(b["code"], "fresh_login_required");
    assert!(
        b["message"].as_str().unwrap().contains("Log in again"),
        "{b}"
    );
    assert_eq!(
        b["login_url"],
        "/v1/auth/login?next=/account/security%3Fenroll%3D1"
    );

    // Logging in again restarts the clock.
    let (s2, _) = browser_login(&h, "amy-code", "/").await;
    let mut soft = passkey();
    h.enroll(&s2, &mut soft, "Laptop").await;
}

/// An API/CLI login (`POST /v1/auth/login`, which also sets cookies) can
/// never enroll a first passkey.
#[tokio::test]
async fn an_api_login_can_never_enroll_the_first_passkey() {
    let h = harness();
    let s = h.session(owner("amy"), LOGIN_KIND_API).await;
    let (st, b) = registration_options(&h, &s).await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::FORBIDDEN, json!("fresh_login_required"))
    );

    // The real exchange stamps `api` too.
    let (st, login) = h
        .send(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(json!({"credential": "amy-code"}).to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let s = Sess {
        id: login["session"].as_str().unwrap().into(),
        csrf: login["csrf"].as_str().unwrap().into(),
    };
    let (st, b) = registration_options(&h, &s).await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::FORBIDDEN, json!("fresh_login_required"))
    );
}

/// After the first, adding a passkey is always-ask (`passkey.add`): an open
/// window does not satisfy it, an action confirmation for exactly that add
/// does. Removing one is always-ask too.
#[tokio::test]
async fn later_passkeys_and_removals_are_always_ask() {
    let h = harness();
    let (s, _) = browser_login(&h, "amy-code", "/").await;
    let mut first = passkey();
    let dto = h.enroll(&s, &mut first, "Laptop").await;
    h.open_window(&s, &mut first).await;

    let (st, b) = registration_options(&h, &s).await;
    assert_step_up(st, &b, "action");
    assert_eq!(b["op"], "passkey.add");
    assert_eq!(b["summary"]["changes"][0], "add passkey `Laptop`");
    let (st, conf) = h.answer(&s, &mut first, &b["challenge"]).await;
    assert_eq!(
        (st, conf["kind"].clone()),
        (StatusCode::OK, json!("action"))
    );
    let (st, opts) = registration_options(&h, &s).await;
    assert_eq!(st, StatusCode::OK, "{opts}");
    let mut second = passkey();
    h.finish_enroll(&s, &mut second, &opts).await;
    let enrolled = h.events_of("passkey.enrolled");
    assert_eq!(enrolled.len(), 2);
    assert_eq!(enrolled[1].detail["first"], false);

    // Removal: refused even inside the window, then confirmed for itself.
    let uri = format!("/v1/me/passkeys/{}", dto["id"].as_str().unwrap());
    let (st, b) = h.cookie("DELETE", &uri, None, &s).await;
    assert_step_up(st, &b, "action");
    assert_eq!(b["op"], "passkey.remove");
    let (st, _) = h.answer(&s, &mut second, &b["challenge"]).await;
    assert_eq!(st, StatusCode::OK);
    let (st, b) = h.cookie("DELETE", &uri, None, &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(h.events_of("passkey.removed").len(), 1);
    let (_, me) = h.cookie("GET", "/v1/me/security", None, &s).await;
    assert_eq!(me["passkeys"].as_array().unwrap().len(), 1);
    assert_eq!(me["passkeys"][0]["label"], "Laptop");
}

/// A login — however fresh — is never a step-up, and enrollment is prompted,
/// not forced: a user without a passkey logs in and reads normally, and is
/// pointed at enrollment only by a crown jewel.
#[tokio::test]
async fn login_is_never_a_step_up_and_enrollment_is_never_forced() {
    let h = harness();
    let (s, _) = browser_login(&h, "amy-code", "/").await;
    let (st, me) = h.cookie("GET", "/v1/me", None, &s).await;
    assert_eq!(st, StatusCode::OK, "{me}");
    let (st, sec) = h.cookie("GET", "/v1/me/security", None, &s).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sec["has_passkey"], false);
    assert_eq!(sec["can_enroll_first"], true);

    // A crown jewel right after a fresh login: still a step-up (enroll).
    let secret = json!({"org": "acme", "name": "X", "value": "v"});
    let (st, b) = h
        .cookie("POST", "/v1/secrets", Some(secret.clone()), &s)
        .await;
    assert_step_up(st, &b, "enroll");
    assert!(b["fresh_login_until"].as_i64().unwrap() > h.now().await);

    // With a passkey, a brand-new login is still not a step-up.
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    let (fresh, _) = browser_login(&h, "amy-code", "/").await;
    let (st, b) = h.cookie("POST", "/v1/secrets", Some(secret), &fresh).await;
    assert_step_up(st, &b, "window");
}
