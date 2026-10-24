//! ADR-0073 C1 (server side): passkey registration and assertion through the
//! real endpoints with a software authenticator — challenges single-use and
//! expiring, rpId/origin bound to the public URL, the counter/clone signal,
//! per-Principal storage with label and last use — and the StepUpConfirmation
//! window: bound to THAT browser session, 5 minutes from confirming, never
//! extended.

mod step_up_harness;

use axum::http::StatusCode;
use serde_json::json;

use scarab_identity::PasskeyStore;
use scarab_webauthn::soft::Counter;

use step_up_harness::*;

const MIN: i64 = 60 * 1000;

fn secret(n: u32) -> serde_json::Value {
    json!({"org": "acme", "name": format!("S{n}"), "value": "v"})
}

/// Window: confirm at t; a crown jewel at t+4:00 and t+4:59 runs, the one at
/// t+4:00 does not extend it, and at t+5:00 it is refused again. The
/// operation is recorded as authorised by that confirmation, then completed.
#[tokio::test]
async fn a_window_lasts_five_minutes_from_confirming_and_is_never_extended() {
    let h = harness();
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;

    // Without a window: refused with a ready window challenge.
    let (st, b) = h.cookie("POST", "/v1/secrets", Some(secret(1)), &s).await;
    assert_step_up(st, &b, "window");
    let (st, conf) = h.answer(&s, &mut soft, &b["challenge"]).await;
    assert_eq!(st, StatusCode::OK, "{conf}");
    let t = h.now().await;
    assert_eq!(conf["kind"], "window");
    assert_eq!(conf["expires_at"], t + 5 * MIN);
    let confirmation_id = conf["confirmation_id"].as_str().unwrap().to_string();

    let (st, b) = h.cookie("POST", "/v1/secrets", Some(secret(1)), &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");

    h.clock.advance(4 * MIN);
    let (st, _) = h.cookie("POST", "/v1/secrets", Some(secret(2)), &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "t+4:00 is inside the window");

    h.clock.advance(59 * 1000);
    let (st, _) = h.cookie("POST", "/v1/secrets", Some(secret(3)), &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "t+4:59 is inside the window");

    h.clock.advance(1000);
    let (st, b) = h.cookie("POST", "/v1/secrets", Some(secret(4)), &s).await;
    assert_step_up(st, &b, "window");

    // Recorded: the confirmation (with its passkey and session), and each
    // operation authorised by it, then completed.
    let confirmed = h.events_of("step_up.confirmed");
    assert_eq!(confirmed.len(), 1);
    assert_eq!(
        confirmed[0].confirmation_id.as_deref(),
        Some(confirmation_id.as_str())
    );
    assert!(confirmed[0].passkey_id.is_some() && confirmed[0].session_hash.is_some());
    let authorised = h.events_of("op.authorised");
    assert_eq!(authorised.len(), 3);
    assert!(authorised.iter().all(|e| e.confirmation_id.as_deref()
        == Some(confirmation_id.as_str())
        && e.op.as_deref() == Some("secret.put")
        && e.org.as_deref() == Some("acme")));
    let completed = h.events_of("op.completed");
    assert_eq!(completed.len(), 3);
    assert!(completed.iter().all(|e| e.detail["status"] == 204));
}

/// The window belongs to the browser session that made it: the same session
/// id presented as Bearer is refused, and so is a second session of the same
/// human.
#[tokio::test]
async fn a_window_covers_only_its_own_cookie_session() {
    let h = harness();
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    h.open_window(&s, &mut soft).await;
    let (st, _) = h.cookie("POST", "/v1/secrets", Some(secret(1)), &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // The very same session id, as Bearer (a CLI, or a stolen id).
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret(2)), &s.id)
        .await;
    assert_step_up(st, &b, "window");
    assert!(
        b.get("challenge").is_none(),
        "no browser ceremony for Bearer: {b}"
    );
    let bearer_refusal = h.events_of("step_up.refused");
    assert_eq!(bearer_refusal.last().unwrap().caller, "session:bearer");

    // Another session of the same human.
    let other = h.browser_session(owner("amy")).await;
    let (st, b) = h
        .cookie("POST", "/v1/secrets", Some(secret(3)), &other)
        .await;
    assert_step_up(st, &b, "window");
}

/// A challenge is answered once, within 2 minutes, in the session it was
/// minted for.
#[tokio::test]
async fn a_challenge_is_single_use_expiring_and_session_bound() {
    let h = harness();
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;

    // Twice.
    let (_, ch) = h.cookie("POST", "/v1/me/step-up/options", None, &s).await;
    let (st, _) = h.answer(&s, &mut soft, &ch).await;
    assert_eq!(st, StatusCode::OK);
    let (st, b) = h.answer(&s, &mut soft, &ch).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["code"], "challenge_invalid");

    // Expired: +2:00 and 1 ms.
    let (_, ch) = h.cookie("POST", "/v1/me/step-up/options", None, &s).await;
    h.clock.advance(2 * MIN + 1);
    let (st, b) = h.answer(&s, &mut soft, &ch).await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("challenge_invalid"))
    );

    // Another session of the same human cannot answer it.
    let (_, ch) = h.cookie("POST", "/v1/me/step-up/options", None, &s).await;
    let other = h.browser_session(owner("amy")).await;
    let (st, b) = h.answer(&other, &mut soft, &ch).await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("challenge_invalid"))
    );
}

/// With `public_url = https://ci.example.com`, an authenticator signing for
/// another origin or another rpId is refused — at enrollment and at step-up.
#[tokio::test]
async fn the_relying_party_is_the_public_url() {
    let h = harness();
    let s = h.browser_session(owner("amy")).await;

    // Enrollment from the wrong origin.
    let mut evil = passkey();
    evil.override_origin = Some("https://evil.example".into());
    let (_, opts) = h
        .cookie(
            "POST",
            "/v1/me/passkeys/registration/options",
            Some(json!({"label": "x"})),
            &s,
        )
        .await;
    let cred = evil.register(&opts["public_key"], ORIGIN).unwrap();
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/me/passkeys/registration",
            Some(json!({"challenge_id": opts["id"], "credential": cred})),
            &s,
        )
        .await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("origin_mismatch"))
    );

    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;

    soft.override_origin = Some("https://evil.example".into());
    let (_, ch) = h.cookie("POST", "/v1/me/step-up/options", None, &s).await;
    let (st, b) = h.answer(&s, &mut soft, &ch).await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("origin_mismatch"))
    );

    soft.override_origin = None;
    soft.override_rp_id = Some("evil.example".into());
    let (_, ch) = h.cookie("POST", "/v1/me/step-up/options", None, &s).await;
    let (st, b) = h.answer(&s, &mut soft, &ch).await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("rp_id_hash_mismatch"))
    );
    assert!(h.events_of("step_up.confirmed").is_empty());
}

/// A counter that goes backwards flags the passkey for good, records
/// `passkey.clone_suspected`, and refuses it — then and ever after.
#[tokio::test]
async fn a_counter_regression_flags_the_passkey() {
    let h = harness();
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    h.open_window(&s, &mut soft).await; // count 1
    h.open_window(&s, &mut soft).await; // count 2

    soft.counter = Counter::Fixed(1);
    let (_, ch) = h.cookie("POST", "/v1/me/step-up/options", None, &s).await;
    let (st, b) = h.answer(&s, &mut soft, &ch).await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::FORBIDDEN, json!("clone_suspected"))
    );
    let flagged = h.passkeys.list("amy").await.unwrap();
    assert!(flagged[0].clone_suspected_at.is_some());
    assert_eq!(h.events_of("passkey.clone_suspected").len(), 1);

    // Never again, even with a counter that moves forward.
    soft.counter = Counter::Incrementing;
    let (st, b) = h.cookie("POST", "/v1/me/step-up/options", None, &s).await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("no_passkey"))
    );
    let (st, b) = h.cookie("GET", "/v1/me/security", None, &s).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b["has_passkey"], false);
    assert_eq!(b["passkeys"][0]["clone_suspected"], true);
}

/// Stored per Principal, with its label; `last_used_at` moves on assertion;
/// a counter that stays 0 (a synced platform passkey) is fine.
#[tokio::test]
async fn a_passkey_is_stored_with_its_label_and_last_use() {
    let h = harness();
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    soft.counter = Counter::AlwaysZero;
    let dto = h.enroll(&s, &mut soft, "  MacBook Touch ID ").await;
    assert_eq!(dto["label"], "MacBook Touch ID");
    assert_eq!(dto["last_used_at"], serde_json::Value::Null);

    let stored = h.passkeys.list("amy").await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].principal, "amy");
    assert_eq!(stored[0].label, "MacBook Touch ID");
    assert!(h.passkeys.list("bob").await.unwrap().is_empty());

    h.clock.advance(MIN);
    h.open_window(&s, &mut soft).await;
    h.clock.advance(MIN);
    h.open_window(&s, &mut soft).await; // 0 → 0 again is accepted
    let t = h.now().await;
    let (_, me) = h.cookie("GET", "/v1/me/security", None, &s).await;
    assert_eq!(me["passkeys"][0]["last_used_at"], t);
    assert_eq!(me["has_passkey"], true);
    assert_eq!(me["window_expires_at"], t + 5 * MIN);
    assert_eq!(me["step_up"], "enforce");

    let enrolled = h.events_of("passkey.enrolled");
    assert_eq!(enrolled.len(), 1);
    assert_eq!(enrolled[0].detail["first"], true);
}

/// amy in her browser with two passkeys: `k1` (enrolled first) and `k2`
/// (a later passkey: always-ask, confirmed with k1). Returns the session,
/// both authenticators and k1's id.
async fn amy_with_two_passkeys(
    h: &Harness,
) -> (
    Sess,
    scarab_webauthn::soft::SoftPasskey,
    scarab_webauthn::soft::SoftPasskey,
    String,
) {
    let s = h.browser_session(owner("amy")).await;
    let mut k1 = passkey();
    let dto = h.enroll(&s, &mut k1, "k1").await;
    let k1_id = dto["id"].as_str().unwrap().to_string();
    let add = json!({"label": "k2"});
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/me/passkeys/registration/options",
            Some(add.clone()),
            &s,
        )
        .await;
    assert_step_up(st, &b, "action");
    h.answer(&s, &mut k1, &b["challenge"]).await;
    let (st, opts) = h
        .cookie(
            "POST",
            "/v1/me/passkeys/registration/options",
            Some(add),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{opts}");
    let mut k2 = passkey();
    h.finish_enroll(&s, &mut k2, &opts).await;
    (s, k1, k2, k1_id)
}

/// git-bug 6856659: removing a passkey revokes the window it opened — a
/// crown jewel in that session is asked again at once, not for up to 5
/// minutes more.
#[tokio::test]
async fn removing_a_passkey_revokes_the_window_it_opened() {
    let h = harness();
    let (s, mut k1, mut k2, k1_id) = amy_with_two_passkeys(&h).await;
    h.open_window(&s, &mut k1).await;
    let (st, _) = h.cookie("POST", "/v1/secrets", Some(secret(1)), &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "the window is open");

    let uri = format!("/v1/me/passkeys/{k1_id}");
    let (st, b) = h.cookie("DELETE", &uri, None, &s).await;
    assert_step_up(st, &b, "action");
    h.answer(&s, &mut k2, &b["challenge"]).await;
    let (st, b) = h.cookie("DELETE", &uri, None, &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");

    let (st, b) = h.cookie("POST", "/v1/secrets", Some(secret(2)), &s).await;
    assert_step_up(st, &b, "window");
    let removed = h.events_of("passkey.removed");
    assert_eq!(removed[0].detail["confirmations_revoked"], 1);
}

/// git-bug 6856659: a passkey flagged as a suspected clone loses the window
/// it opened (and any unconsumed action confirmation): whoever holds the
/// copy keeps nothing it signed.
#[tokio::test]
async fn a_clone_flag_revokes_the_window_the_passkey_opened() {
    let h = harness();
    let (s, mut k1, _k2, _) = amy_with_two_passkeys(&h).await;
    h.open_window(&s, &mut k1).await;
    let (st, _) = h.cookie("POST", "/v1/secrets", Some(secret(1)), &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "the window is open");

    k1.counter = Counter::Fixed(1);
    let (_, ch) = h.cookie("POST", "/v1/me/step-up/options", None, &s).await;
    let (st, b) = h.answer(&s, &mut k1, &ch).await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::FORBIDDEN, json!("clone_suspected"))
    );

    let (st, b) = h.cookie("POST", "/v1/secrets", Some(secret(2)), &s).await;
    assert_step_up(st, &b, "window");
    let flagged = h.events_of("passkey.clone_suspected");
    assert_eq!(flagged[0].detail["confirmations_revoked"], 1);
}
