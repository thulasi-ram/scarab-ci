//! ADR-0073 §7, §8, §10 (C7, C8): the OrgSecurityEventLog as the API serves
//! it — every confirmation, operation, refusal and passkey enrollment /
//! removal / reset carries the links that make it evidence; an Org's admins
//! read their Org's log (and their members' passkey events) and nobody
//! else's; a user reads their own; the Org summary — and the two passkey
//! resets: an Owner's (always-ask, Owner of every Org the member is in) and
//! the operator's break-glass straight at the database.
//!
//! The Org view joins live role bindings, so the visibility test runs on the
//! in-memory stores AND on a real Postgres (`fresh_db`; skipped without
//! `SCARAB_TEST_DATABASE_URL`); the break-glass is Postgres-only.

mod common;
mod step_up_harness;

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{json, Value};

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{
    api_token_hash, ApiToken, ApiTokenStore, IdentityStore, PasskeyStore, ProviderIdentity, Role,
    Scope, SecurityEventFilter, SecurityEventLog, StepUpStore,
};
use scarab_server::step_up::admin::{reset_passkeys, OperatorInfo};
use scarab_webauthn::soft::SoftPasskey;

use step_up_harness::*;

const HOUR: i64 = 60 * 60 * 1000;

async fn pg() -> Option<(common::TestDb, Arc<PostgresDb>)> {
    let tdb = fresh_db().await?;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    Some((tdb, pg))
}

fn secret(org: &str, value: &str) -> Value {
    json!({"org": org, "name": "DB_PASSWORD", "value": value})
}

/// `subject` (no global role) signed in from a browser with a passkey,
/// holding `role` in `org`.
async fn member(h: &Harness, subject: &str, org: &str, role: Role) -> (Sess, SoftPasskey) {
    h.grant(subject, Scope::Org(org.into()), role).await;
    let s = h.browser_session(scoped(subject)).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, &format!("{subject}'s key")).await;
    (s, soft)
}

fn kinds(page: &Value) -> Vec<String> {
    page["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect()
}

fn events_about<'a>(page: &'a Value, subject: &str) -> Vec<&'a Value> {
    page["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["subject"] == subject)
        .collect()
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// Each kind of event carries what makes it evidence: a confirmation its
/// passkey and session; an operation the confirmation that authorised it; a
/// refusal the caller and the operation; an enrollment / removal the
/// passkey; a removal also its always-ask confirmation.
#[tokio::test]
async fn every_event_carries_its_links() {
    let h = harness();
    let (amy, mut soft) = member(&h, "amy", "acme", Role::Owner).await;

    // A refusal, then a window, then the operation it authorises.
    let (st, b) = h
        .cookie("POST", "/v1/secrets", Some(secret("acme", "v")), &amy)
        .await;
    assert_step_up(st, &b, "window");
    let (st, conf) = h.answer(&amy, &mut soft, &b["challenge"]).await;
    assert_eq!(st, StatusCode::OK, "{conf}");
    let (st, _) = h
        .cookie("POST", "/v1/secrets", Some(secret("acme", "v")), &amy)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (st, page) = h
        .cookie("GET", "/v1/me/security/events?limit=200", None, &amy)
        .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    let find = |kind: &str| -> Vec<Value> {
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == kind)
            .cloned()
            .collect()
    };
    let enrolled = &find("passkey.enrolled")[0];
    let passkey_id = enrolled["passkey_id"].as_str().expect("passkey linked");
    assert!(enrolled["session_hash"].is_string());

    let refused = &find("step_up.refused")[0];
    assert_eq!(refused["caller"], "session:cookie");
    assert_eq!(refused["op"], "secret.put");
    assert_eq!(refused["org"], "acme");
    assert_eq!(refused["target"], "acme/DB_PASSWORD");

    let confirmed = &find("step_up.confirmed")[0];
    assert_eq!(confirmed["confirmation_id"], conf["confirmation_id"]);
    assert_eq!(confirmed["passkey_id"], passkey_id);
    assert!(confirmed["session_hash"].is_string());
    assert_eq!(confirmed["detail"]["kind"], "window");

    for kind in ["op.authorised", "op.completed"] {
        let op = &find(kind)[0];
        assert_eq!(op["op"], "secret.put", "{kind}");
        assert_eq!(op["confirmation_id"], conf["confirmation_id"], "{kind}");
        assert_eq!(op["org"], "acme", "{kind}");
    }

    // Removing a passkey: always-ask; the removal links its confirmation.
    let mut spare = passkey();
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/me/passkeys/registration/options",
            Some(json!({"label": "Spare"})),
            &amy,
        )
        .await;
    assert_step_up(st, &b, "action");
    h.answer(&amy, &mut soft, &b["challenge"]).await;
    let (_, opts) = h
        .cookie(
            "POST",
            "/v1/me/passkeys/registration/options",
            Some(json!({"label": "Spare"})),
            &amy,
        )
        .await;
    let spare_dto = h.finish_enroll(&amy, &mut spare, &opts).await;
    let uri = format!("/v1/me/passkeys/{}", spare_dto["id"].as_str().unwrap());
    let (st, b) = h.cookie("DELETE", &uri, None, &amy).await;
    assert_step_up(st, &b, "action");
    let (_, conf) = h.answer(&amy, &mut soft, &b["challenge"]).await;
    let (st, _) = h.cookie("DELETE", &uri, None, &amy).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let removed = h.events("passkey.removed").await;
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].passkey_id.as_deref(), spare_dto["id"].as_str());
    assert_eq!(
        removed[0].confirmation_id.as_deref(),
        conf["confirmation_id"].as_str(),
        "the removal links the always-ask confirmation that allowed it"
    );
}

// ---------------------------------------------------------------------------
// Visibility
// ---------------------------------------------------------------------------

/// Org A's admin reads Org A's log — its own events and Org A members'
/// passkey and confirmation events — and never Org B's. A member who is
/// not an admin is refused; everyone reads their own; only a global
/// administrator reads the install.
async fn the_org_log_shows_its_org_and_members_only(backend: Backend) {
    let h = harness_on(backend, |st| st);
    let (amy, mut amy_key) = member(&h, "amy", "acme", Role::Owner).await;
    let (carl, mut carl_key) = member(&h, "carl", "beta", Role::Owner).await;
    let (dave, _) = member(&h, "dave", "acme", Role::Member).await;
    for (s, key, org) in [(&amy, &mut amy_key, "acme"), (&carl, &mut carl_key, "beta")] {
        h.open_window(s, key).await;
        let (st, _) = h
            .cookie("POST", "/v1/secrets", Some(secret(org, "v")), s)
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
    }

    let (st, acme) = h
        .cookie("GET", "/v1/orgs/acme/security/events?limit=200", None, &amy)
        .await;
    assert_eq!(st, StatusCode::OK, "{acme}");
    for e in acme["events"].as_array().unwrap() {
        assert!(
            e["org"] == "acme"
                || (e["org"].is_null() && (e["subject"] == "amy" || e["subject"] == "dave")),
            "acme's log leaked {e}"
        );
    }
    assert!(events_about(&acme, "carl").is_empty());
    let k = kinds(&acme);
    assert!(k.contains(&"op.authorised".to_string()));
    assert!(
        events_about(&acme, "dave")
            .iter()
            .any(|e| e["kind"] == "passkey.enrolled"),
        "a member's passkey events are in their Org's log"
    );
    assert!(events_about(&acme, "amy")
        .iter()
        .any(|e| e["kind"] == "step_up.confirmed"));

    // amy is no admin of beta; dave is no admin of acme.
    let (st, _) = h
        .cookie("GET", "/v1/orgs/beta/security/events", None, &amy)
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = h
        .cookie("GET", "/v1/orgs/acme/security/events", None, &dave)
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = h
        .cookie("GET", "/v1/orgs/acme/security/summary", None, &dave)
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // Everyone reads their own.
    let (st, mine) = h.cookie("GET", "/v1/me/security/events", None, &dave).await;
    assert_eq!(st, StatusCode::OK, "{mine}");
    assert!(!mine["events"].as_array().unwrap().is_empty());
    assert!(mine["events"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["subject"] == "dave"));

    // The install-wide log: a global administrator only.
    let (st, _) = h.cookie("GET", "/v1/security/events", None, &amy).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let root = h.browser_session(owner("root")).await;
    let (st, all) = h
        .cookie("GET", "/v1/security/events?limit=200", None, &root)
        .await;
    assert_eq!(st, StatusCode::OK, "{all}");
    assert!(!events_about(&all, "carl").is_empty() && !events_about(&all, "amy").is_empty());

    // Paging and the kind filter.
    let (_, p1) = h
        .cookie(
            "GET",
            "/v1/security/events?kind=passkey.enrolled&limit=2",
            None,
            &root,
        )
        .await;
    assert_eq!(kinds(&p1), vec!["passkey.enrolled"; 2]);
    let before = p1["next_before"].as_i64().expect("a next page");
    let (_, p2) = h
        .cookie(
            "GET",
            &format!("/v1/security/events?kind=passkey.enrolled&limit=2&before={before}"),
            None,
            &root,
        )
        .await;
    assert_eq!(kinds(&p2), vec!["passkey.enrolled"], "3 enrolled in all");
    assert!(p2["next_before"].is_null());
    assert!(p2["events"][0]["seq"].as_i64().unwrap() < before);
}

#[tokio::test]
async fn the_org_log_shows_its_org_and_members_only_in_memory() {
    the_org_log_shows_its_org_and_members_only(Backend::Memory).await;
}

#[tokio::test]
async fn the_org_log_shows_its_org_and_members_only_on_postgres() {
    let Some((tdb, pg)) = pg().await else { return };
    the_org_log_shows_its_org_and_members_only(Backend::Pg(pg)).await;
    tdb.cleanup().await;
}

/// The Org summary: who among its admins has no passkey, recent refusals
/// (24 hours), waiting requests — what the banner reads.
#[tokio::test]
async fn the_org_summary_counts_admins_without_a_passkey() {
    let h = harness();
    let (amy, _) = member(&h, "amy", "acme", Role::Owner).await;
    h.grant("bob", Scope::Org("acme".into()), Role::Admin).await;
    h.grant(
        "erin",
        Scope::Project {
            org: "acme".into(),
            name: "web".into(),
        },
        Role::Admin,
    )
    .await;
    let (st, b) = h
        .cookie("POST", "/v1/secrets", Some(secret("acme", "v")), &amy)
        .await;
    assert_step_up(st, &b, "window");
    // A token of amy's raises a request in acme.
    let plaintext = "scarab_pat_t1_0123456789abcdef0123456789abcdef";
    let now = h.now().await;
    h.tokens
        .put(
            &ApiToken {
                id: "t1".into(),
                name: "ci".into(),
                owner_subject: "amy".into(),
                scope: Scope::Org("acme".into()),
                role: Role::Admin,
                expires_at: now + 24 * HOUR,
                created_by: "amy".into(),
                created_at: now,
                last_used_at: None,
                revoked_at: None,
            },
            &api_token_hash(plaintext),
        )
        .await
        .unwrap();
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret("acme", "v")), plaintext)
        .await;
    assert!(b["request_id"].is_string(), "{st} {b}");

    let (st, sum) = h
        .cookie("GET", "/v1/orgs/acme/security/summary", None, &amy)
        .await;
    assert_eq!(st, StatusCode::OK, "{sum}");
    assert_eq!(sum["enforced"], true);
    assert_eq!(sum["caller_role"], "owner");
    assert_eq!(sum["admins_total"], 2, "Org-scope Admin/Owner bindings");
    assert_eq!(sum["admins_without_passkey"], json!(["bob"]));
    assert_eq!(sum["pending_requests"], 1);
    let refusals = sum["recent_refusals"].as_array().unwrap();
    assert_eq!(refusals.len(), 2, "{sum}");
    assert!(refusals.iter().all(|e| e["kind"] == "step_up.refused"));

    h.clock.advance(25 * HOUR);
    let (_, sum) = h
        .cookie("GET", "/v1/orgs/acme/security/summary", None, &amy)
        .await;
    assert!(sum["recent_refusals"].as_array().unwrap().is_empty());
    assert_eq!(sum["pending_requests"], 0, "expired requests do not wait");
}

// ---------------------------------------------------------------------------
// An Owner's reset
// ---------------------------------------------------------------------------

/// An Owner resets a member's passkeys: always-ask; every passkey and
/// standing step-up of theirs goes; `passkey.reset` links the confirmation.
#[tokio::test]
async fn an_owner_resets_a_member_s_passkeys() {
    let h = harness();
    let (amy, mut soft) = member(&h, "amy", "acme", Role::Owner).await;
    let (bob, mut bob_key) = member(&h, "bob", "acme", Role::Admin).await;
    h.open_window(&bob, &mut bob_key).await;
    let uri = "/v1/orgs/acme/members/bob/passkeys/reset";

    h.open_window(&amy, &mut soft).await;
    let (st, b) = h.cookie("POST", uri, None, &amy).await;
    assert_step_up(st, &b, "action");
    assert_eq!(b["op"], "passkey.reset");
    let (_, conf) = h.answer(&amy, &mut soft, &b["challenge"]).await;
    let (st, done) = h.cookie("POST", uri, None, &amy).await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["removed"], 1);
    assert_eq!(done["windows_revoked"], 1);
    assert!(h.passkey_store.list("bob").await.unwrap().is_empty());
    assert!(h
        .requests
        .window_expiry(&session_hash(&bob.id), "bob", h.now().await)
        .await
        .unwrap()
        .is_none());

    let reset = h.events("passkey.reset").await;
    assert_eq!(reset.len(), 1);
    assert_eq!(reset[0].subject, "bob");
    assert_eq!(reset[0].org.as_deref(), Some("acme"));
    assert_eq!(reset[0].caller, "session:cookie");
    assert_eq!(
        reset[0].confirmation_id.as_deref(),
        conf["confirmation_id"].as_str()
    );
    assert_eq!(reset[0].detail["reset_by"], "amy");

    // bob's next passkey is a first one again: a stale login cannot add it.
    h.clock.advance(6 * 60 * 1000);
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/me/passkeys/registration/options",
            Some(json!({"label": "New"})),
            &bob,
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["code"], "fresh_login_required");
}

/// A member who also belongs to an Org the caller does not own cannot be
/// reset by them (403 naming that Org); an Admin is no Owner; a non-member
/// is 404.
#[tokio::test]
async fn an_owner_cannot_reset_a_member_of_an_org_they_do_not_own() {
    let h = harness();
    let (amy, mut soft) = member(&h, "amy", "acme", Role::Owner).await;
    let (bob, _) = member(&h, "bob", "acme", Role::Admin).await;
    h.grant("bob", Scope::Org("beta".into()), Role::Admin).await;
    let uri = "/v1/orgs/acme/members/bob/passkeys/reset";

    let (st, b) = h.cookie("POST", uri, None, &amy).await;
    assert_step_up(st, &b, "action");
    h.answer(&amy, &mut soft, &b["challenge"]).await;
    let (st, b) = h.cookie("POST", uri, None, &amy).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    let text = b.as_str().unwrap();
    assert!(
        text.contains("beta") && text.contains("Owner of every Org"),
        "{text}"
    );
    assert_eq!(h.passkey_store.list("bob").await.unwrap().len(), 1);
    assert!(h.events("passkey.reset").await.is_empty());

    // Owner of beta too: now it goes through.
    h.grant("amy", Scope::Org("beta".into()), Role::Owner).await;
    let (st, b) = h.cookie("POST", uri, None, &amy).await;
    assert_step_up(st, &b, "action");
    h.answer(&amy, &mut soft, &b["challenge"]).await;
    let (st, b) = h.cookie("POST", uri, None, &amy).await;
    assert_eq!(st, StatusCode::OK, "{b}");

    // An Admin is not an Owner (refused for the role, before any step-up).
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/members/amy/passkeys/reset",
            None,
            &bob,
        )
        .await;
    assert_eq!(
        (st, b),
        (
            StatusCode::FORBIDDEN,
            Value::String("insufficient role".into())
        )
    );

    // Not a member of the Org.
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/members/zed/passkeys/reset",
            None,
            &amy,
        )
        .await;
    assert_step_up(st, &b, "action");
    h.answer(&amy, &mut soft, &b["challenge"]).await;
    let (st, _) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/members/zed/passkeys/reset",
            None,
            &amy,
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// How the server keys a session's confirmations: the session id's hash.
fn session_hash(id: &str) -> String {
    api_token_hash(id)
}

// ---------------------------------------------------------------------------
// The operator break-glass
// ---------------------------------------------------------------------------

/// `scarab-server admin reset-passkeys`: removes every passkey of the
/// principal and its standing step-ups, records `passkey.break_glass_reset`
/// (Org-less, caller `operator:break-glass`, host, user and reason) — and
/// refuses a database whose schema predates the event log, without
/// migrating it.
#[tokio::test]
async fn the_break_glass_resets_and_is_recorded() {
    let Some((tdb, pg)) = pg().await else { return };
    let h = harness_on(Backend::Pg(pg.clone()), |st| st);
    // amy is a Principal (she signed in once).
    let amy_id = IdentityStore::sign_in(
        pg.as_ref(),
        &ProviderIdentity {
            provider: "github".into(),
            external_id: "1".into(),
            handle: "amy".into(),
            email: None,
            email_verified: false,
            display_name: None,
        },
        "amy",
        T0,
    )
    .await
    .unwrap();
    assert_eq!(amy_id, "amy");
    let (amy, mut soft) = member(&h, "amy", "acme", Role::Owner).await;
    h.open_window(&amy, &mut soft).await;

    let operator = OperatorInfo {
        host: "ops-1".into(),
        os_user: "root".into(),
    };
    let now = h.now().await;
    let done = reset_passkeys(&pg, "amy", Some("lost laptop"), &operator, now)
        .await
        .unwrap();
    assert_eq!((done.subject.as_str(), done.removed), ("amy", 1));
    assert_eq!(done.windows_revoked, 1);
    assert!(PasskeyStore::list(pg.as_ref(), "amy")
        .await
        .unwrap()
        .is_empty());
    assert!(
        !StepUpStore::has_window(pg.as_ref(), &session_hash(&amy.id), "amy", now)
            .await
            .unwrap()
    );

    let events = SecurityEventLog::list(
        pg.as_ref(),
        &SecurityEventFilter {
            kinds: vec!["passkey.break_glass_reset".into()],
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.subject, "amy");
    assert_eq!(e.org, None);
    assert_eq!(e.caller, "operator:break-glass");
    assert_eq!(e.detail["host"], "ops-1");
    assert_eq!(e.detail["os_user"], "root");
    assert_eq!(e.detail["reason"], "lost laptop");
    assert_eq!(e.detail["removed"], 1);
    // amy's admins see it in acme's log (she is a member).
    let (st, acme) = {
        let s = h.browser_session(scoped("amy")).await;
        h.cookie(
            "GET",
            "/v1/orgs/acme/security/events?kind=passkey.break_glass_reset",
            None,
            &s,
        )
        .await
    };
    assert_eq!(st, StatusCode::OK, "{acme}");
    assert_eq!(kinds(&acme), vec!["passkey.break_glass_reset"]);

    // No such principal.
    let err = reset_passkeys(&pg, "nobody", None, &operator, now)
        .await
        .unwrap_err();
    assert!(err.contains("no principal"), "{err}");

    // A schema older than the event log: refused, and NOT migrated.
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version >= 62")
        .execute(&tdb.pool)
        .await
        .unwrap();
    let err = reset_passkeys(&pg, "amy", None, &operator, now)
        .await
        .unwrap_err();
    assert!(err.contains("never migrates"), "{err}");
    assert!(
        pg.schema_version().await.unwrap().unwrap() < 62,
        "untouched"
    );
    tdb.cleanup().await;
}

/// A database never migrated at all is refused too.
#[tokio::test]
async fn the_break_glass_refuses_an_unmigrated_database() {
    let Some(tdb) = fresh_db().await else { return };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    let err = reset_passkeys(
        &pg,
        "amy",
        None,
        &OperatorInfo {
            host: "h".into(),
            os_user: "u".into(),
        },
        0,
    )
    .await
    .unwrap_err();
    assert!(err.contains("never been migrated"), "{err}");
    assert_eq!(pg.schema_version().await.unwrap(), None);
    tdb.cleanup().await;
}
