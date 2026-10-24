//! ADR-0073 C4: always-ask. API token creation, plugin install, a
//! PluginConnection's destination, and passkey add/remove each need a
//! confirmation bound to THAT action: the challenge commits to the action's
//! digest, an open window never satisfies it, a confirmation is spent once,
//! and it covers only the exact request it was made for.

mod step_up_harness;

use axum::http::StatusCode;
use serde_json::{json, Value};

use scarab_identity::{Role, Scope};

use step_up_harness::*;

fn mint(role: &str) -> Value {
    json!({"name": "ci-bot", "role": role, "expires_in_days": 7})
}

async fn signed_in_owner(h: &Harness) -> (Sess, scarab_webauthn::soft::SoftPasskey) {
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (s, soft)
}

/// An open window never satisfies an always-ask operation; a confirmation
/// for digest D covers exactly D, once.
#[tokio::test]
async fn token_creation_is_confirmed_per_exact_action_and_only_once() {
    let h = harness();
    let (s, mut soft) = signed_in_owner(&h).await;
    h.open_window(&s, &mut soft).await;

    // Window open, still refused: always-ask.
    let uri = "/v1/orgs/acme/tokens";
    let (st, b) = h.cookie("POST", uri, Some(mint("member")), &s).await;
    assert_step_up(st, &b, "action");
    assert_eq!(b["op"], "token.mint");
    assert_eq!(b["summary"]["target"], "acme/ci-bot");

    // Confirm THIS action (role member)…
    let (st, conf) = h.answer(&s, &mut soft, &b["challenge"]).await;
    assert_eq!(
        (st, conf["kind"].clone()),
        (StatusCode::OK, json!("action"))
    );

    // …a different request (role admin) is not covered…
    let (st, b) = h.cookie("POST", uri, Some(mint("admin")), &s).await;
    assert_step_up(st, &b, "action");

    // …the exact one is, with the keys in another order…
    let reordered = json!({"expires_in_days": 7, "role": "member", "name": "ci-bot"});
    let (st, b) = h.cookie("POST", uri, Some(reordered), &s).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert!(b["token"].as_str().unwrap().starts_with("scarab_pat_"));

    // …and only once.
    let (st, b) = h.cookie("POST", uri, Some(mint("member")), &s).await;
    assert_step_up(st, &b, "action");

    let authorised = h.events_of("op.authorised");
    assert_eq!(authorised.len(), 1);
    assert_eq!(authorised[0].op.as_deref(), Some("token.mint"));
    assert_eq!(
        authorised[0].confirmation_id,
        conf["confirmation_id"].as_str().map(String::from)
    );
}

/// An assertion over one action's challenge cannot be spent on another's:
/// the challenge each signs is derived from its own digest.
#[tokio::test]
async fn an_assertion_over_another_actions_challenge_is_refused() {
    let h = harness();
    let (s, mut soft) = signed_in_owner(&h).await;
    let uri = "/v1/orgs/acme/tokens";
    let (_, d1) = h.cookie("POST", uri, Some(mint("member")), &s).await;
    let (_, d2) = h.cookie("POST", uri, Some(mint("admin")), &s).await;

    // Sign D1's challenge, present it as the answer to D2's.
    let cred = soft.assert(&d1["challenge"]["public_key"], ORIGIN).unwrap();
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/me/step-up",
            Some(json!({"challenge_id": d2["challenge"]["id"], "credential": cred})),
            &s,
        )
        .await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("challenge_mismatch"))
    );

    // Neither action is now confirmed.
    let (st, b) = h.cookie("POST", uri, Some(mint("admin")), &s).await;
    assert_step_up(st, &b, "action");
    let (st, b) = h.cookie("POST", uri, Some(mint("member")), &s).await;
    assert_step_up(st, &b, "action");
}

/// The same pattern for the other always-ask crown jewels: refused inside a
/// window, then let through to the handler by a confirmation of exactly that
/// request — once.
#[tokio::test]
async fn plugin_install_and_plugin_connection_are_always_ask() {
    let h = harness();
    let (s, mut soft) = signed_in_owner(&h).await;
    h.open_window(&s, &mut soft).await;
    let cases = [
        (
            "POST",
            "/v1/orgs/acme/plugins",
            json!({"name": "argo", "plugin": "argocd", "image": "img@sha256:00"}),
            "plugin.install",
        ),
        (
            "PUT",
            "/v1/orgs/acme/plugins/argo/connections/api",
            json!({"base_url": "https://argo.example", "credential": "c"}),
            "plugin_connection.put",
        ),
    ];
    for (m, uri, body, op) in cases {
        let (st, b) = h.cookie(m, uri, Some(body.clone()), &s).await;
        assert_step_up(st, &b, "action");
        assert_eq!(b["op"], op);
        let (st, _) = h.answer(&s, &mut soft, &b["challenge"]).await;
        assert_eq!(st, StatusCode::OK);
        // Past the guard: whatever the handler says (no plugin store is
        // wired here), it is not a step-up refusal.
        let (st, b) = h.cookie(m, uri, Some(body.clone()), &s).await;
        assert!(!is_step_up(st, &b), "{m} {uri}: {st} {b}");
        let (st, b) = h.cookie(m, uri, Some(body), &s).await;
        assert_step_up(st, &b, "action");
    }
}

/// An API token never satisfies a step-up, even when its human holds an open
/// window — and gets no browser challenge.
#[tokio::test]
async fn an_api_token_is_never_a_step_up() {
    let h = harness();
    let (s, mut soft) = signed_in_owner(&h).await;
    // Mint a token (always-ask, confirmed).
    let (_, b) = h
        .cookie("POST", "/v1/orgs/acme/tokens", Some(mint("owner")), &s)
        .await;
    h.answer(&s, &mut soft, &b["challenge"]).await;
    let (st, minted) = h
        .cookie("POST", "/v1/orgs/acme/tokens", Some(mint("owner")), &s)
        .await;
    assert_eq!(st, StatusCode::CREATED, "{minted}");
    let token = minted["token"].as_str().unwrap();
    h.open_window(&s, &mut soft).await;

    let secret = json!({"org": "acme", "name": "X", "value": "v"});
    let (st, _) = h
        .cookie("POST", "/v1/secrets", Some(secret.clone()), &s)
        .await;
    assert_eq!(
        st,
        StatusCode::NO_CONTENT,
        "the human's own browser is inside its window"
    );
    let (st, b) = h.bearer("POST", "/v1/secrets", Some(secret), token).await;
    assert_step_up(st, &b, "window");
    assert!(b.get("challenge").is_none(), "{b}");
    let last = h.events_of("step_up.refused").pop().unwrap();
    assert!(last.caller.starts_with("api_token:"), "{}", last.caller);
}
