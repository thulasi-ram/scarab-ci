//! RED TEAM (auth / step-up / server surface) against fix/open-bugs
//! (`feat/step-up..HEAD`). Every test is an ATTACK with a concrete input or
//! condition.
//!
//! - A test that passes = the attack SURVIVED (the defence held).
//! - A test marked `#[ignore = "redteam: BROKE …"]` FAILS when run: it
//!   demonstrates a real break, kept ignored so the suite stays green. Run
//!   them with `--run-ignored all`.
//!
//! Classical, per ADR-0017: the in-memory twins or a real Postgres
//! (`fresh_db`; skipped without `SCARAB_TEST_DATABASE_URL`); the one fake is
//! the authenticator (a software passkey).

mod common;
mod step_up_harness;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{
    api_token_hash, ApiToken, ApiTokenStore, IdentityStore, PasskeyStore, ProviderIdentity, Role,
    Scope, Session, SessionStore, LOGIN_KIND_BROWSER,
};
use scarab_secrets::{Secret, SecretProvider, SecretScope};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeSecrets, InMemoryDb, InMemoryEnvironmentGovernance,
    InMemoryObjectStore,
};
use scarab_webauthn::soft::{Counter, SoftPasskey};

use step_up_harness::*;

const MIN: i64 = 60 * 1000;
const DAY: i64 = 24 * 60 * MIN;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

async fn pg() -> Option<(common::TestDb, Arc<PostgresDb>)> {
    let tdb = fresh_db().await?;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    Some((tdb, pg))
}

/// An API token of amy's; returns its plaintext.
async fn pat(h: &Harness, id: &str, scope: Scope, role: Role) -> String {
    let plaintext = format!("scarab_pat_{id}_0123456789abcdef0123456789abcdef");
    let now = h.now().await;
    h.tokens
        .put(
            &ApiToken {
                id: id.into(),
                name: format!("ci-{id}"),
                owner_subject: "amy".into(),
                scope,
                role,
                expires_at: now + 30 * DAY,
                created_by: "amy".into(),
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

/// amy (an Owner of acme) in her browser, with one passkey.
async fn amy_in_browser(h: &Harness) -> (Sess, SoftPasskey) {
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (s, soft)
}

/// `subject` (no global role) in a browser with a passkey, `role` in `org`.
async fn member(h: &Harness, subject: &str, org: &str, role: Role) -> (Sess, SoftPasskey) {
    h.grant(subject, Scope::Org(org.into()), role).await;
    let s = h.browser_session(scoped(subject)).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, &format!("{subject}'s key")).await;
    (s, soft)
}

/// amy with two passkeys: k1 (first) and k2 (added with k1's always-ask).
async fn amy_with_two_passkeys(h: &Harness) -> (Sess, SoftPasskey, SoftPasskey, String) {
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

fn secret(org: &str, name: &str) -> Value {
    json!({"org": org, "name": name, "value": "v"})
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// A bare (no-login) app over `secrets`, with org Environment governance
/// wired so the secret matrix answers.
fn bare_app(secrets: Arc<FakeSecrets>) -> axum::Router {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    router(
        AppState::new(db, Arc::new(FakeClock::new(0)), logs)
            .with_secrets(secrets)
            .with_environment_governance(Arc::new(InMemoryEnvironmentGovernance::default())),
    )
}

async fn call(app: &axum::Router, req: Request<Body>) -> (StatusCode, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (st, String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------------------
// ff7a014 / 5d78531 — requests a machine could never execute
// ---------------------------------------------------------------------------

/// ATTACK: a PROJECT-scoped token (acme/web) writes an ORG-level secret of
/// its own Org. `requestable` compares only `token.scope.org()` with the
/// requested Org, so the request is raised — yet the handler checks
/// `token.covers(Scope::Org("acme"))`, which a Project token never does, so
/// whoever confirms it, it can only end `failed`. That is exactly ticket
/// ff7a014's "ops outside the token's scope" (a plausible prompt for a
/// compromised agent to send its human), left open for Project tokens.
#[tokio::test]
async fn rt_project_token_raises_an_org_scope_request_it_can_never_run() {
    let h = harness();
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let token = pat(
        &h,
        "tp",
        Scope::Project {
            org: "acme".into(),
            name: "web".into(),
        },
        Role::Admin,
    )
    .await;
    let (st, b) = h
        .bearer(
            "POST",
            "/v1/secrets",
            Some(secret("acme", "ORG_WIDE")),
            &token,
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert!(
        h.step_up.requests().is_empty(),
        "a request was raised for a write the token can never perform: {b}"
    );
    assert_eq!(b["code"], "outside_token_scope", "{b}");
}

/// Supporting fact for the BROKE above (passes): the request that Project
/// token raises can only ever end `failed` 403 — confirming it runs nothing.
#[tokio::test]
async fn rt_the_project_token_org_scope_request_only_ever_fails() {
    let h = harness();
    let (amy, mut soft) = amy_in_browser(&h).await;
    let token = pat(
        &h,
        "tp",
        Scope::Project {
            org: "acme".into(),
            name: "web".into(),
        },
        Role::Admin,
    )
    .await;
    let (st, b) = h
        .bearer(
            "POST",
            "/v1/secrets",
            Some(secret("acme", "ORG_WIDE")),
            &token,
        )
        .await;
    if b.get("request_id").is_none() {
        // Fixed: nothing raised, nothing to confirm.
        assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
        return;
    }
    let id = b["request_id"].as_str().unwrap().to_string();
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
    assert_eq!(done["state"], "failed", "{done}");
    assert_eq!(done["result_status"], 403, "{done}");
}

/// ATTACK: a token whose CEILING is Member asks for an Admin crown jewel.
/// SURVIVES: `reaches_min_role` caps by the token's ceiling, so no request
/// is raised; the handler answers the plain role refusal.
#[tokio::test]
async fn rt_a_member_ceiling_token_raises_no_admin_request() {
    let h = harness();
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let token = pat(&h, "tm", Scope::Org("acme".into()), Role::Member).await;
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret("acme", "X")), &token)
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert!(!is_step_up(st, &b), "{b}");
    assert!(h.step_up.requests().is_empty());
}

/// ATTACK: an Org token addresses its Org by a case, whitespace or type
/// variant (`ACME`, ` acme`, `acme\u{0}`, a number, an array) — the guard's
/// pointer read vs the handler's typed read. SURVIVES: every variant is
/// `outside_token_scope` (fail closed), and nothing is raised.
#[tokio::test]
async fn rt_org_spelling_variants_never_raise_a_request() {
    let h = harness();
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    for org in [
        json!("ACME"),
        json!(" acme"),
        json!("acme\u{0}"),
        json!("ac%6De"),
        json!(5),
        json!(["acme"]),
        json!(null),
    ] {
        let (st, b) = h
            .bearer(
                "POST",
                "/v1/secrets",
                Some(json!({"org": org, "name": "X", "value": "v"})),
                &token,
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{org}: {b}");
        assert_eq!(b["code"], "outside_token_scope", "{org}: {b}");
    }
    // Duplicate keys: serde_json's Value keeps the LAST, a typed struct
    // refuses duplicates — the guard must not read "acme" for a body the
    // handler reads otherwise. Either way nothing may execute.
    let dup = r#"{"org":"beta","org":"acme","name":"X","value":"v"}"#;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/secrets")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(dup))
        .unwrap();
    let (st, b) = h.send(req).await;
    assert!(st.is_client_error(), "{st} {b}");
    assert!(
        h.step_up.requests().len() <= 1,
        "at most the one ambiguous request"
    );
}

// ---------------------------------------------------------------------------
// e582326 / 549155e — request caps
// ---------------------------------------------------------------------------

/// ATTACK: 30 raises from 3 tokens of one human, fired concurrently at a
/// real Postgres (per-requester cap 10, per-human cap 20). SURVIVES: the
/// advisory lock serializes count-then-insert — exactly 20 are pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn rt_concurrent_raises_never_exceed_the_per_human_cap_on_pg() {
    let Some((tdb, pg)) = pg().await else { return };
    let h = harness_on(Backend::Pg(pg.clone()), |st| st);
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let mut tokens = Vec::new();
    for id in ["c1", "c2", "c3"] {
        tokens.push(pat(&h, id, Scope::Org("acme".into()), Role::Admin).await);
    }
    let mut tasks = Vec::new();
    for (t, token) in tokens.iter().enumerate() {
        for i in 0..10 {
            let app = h.app.clone();
            let token = token.clone();
            tasks.push(tokio::spawn(async move {
                let body = json!({"org": "acme", "name": format!("K{t}_{i}"), "value": "v"});
                let req = Request::builder()
                    .method("POST")
                    .uri("/v1/secrets")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap();
                app.oneshot(req).await.unwrap().status()
            }));
        }
    }
    let mut raised = 0;
    let mut capped = 0;
    for t in tasks {
        match t.await.unwrap() {
            StatusCode::FORBIDDEN => raised += 1,
            StatusCode::TOO_MANY_REQUESTS => capped += 1,
            other => panic!("unexpected {other}"),
        }
    }
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM step_up_requests WHERE state = 'pending'")
            .fetch_one(&tdb.pool)
            .await
            .unwrap();
    assert_eq!(pending, 20, "raised={raised} capped={capped}");
    assert_eq!((raised, capped), (20, 10));
    tdb.cleanup().await;
}

/// ATTACK: the body cap at its exact boundary for `secret.put` (256 KiB).
/// SURVIVES: one byte over raises nothing (413); exactly at the cap raises.
#[tokio::test]
async fn rt_the_body_cap_holds_at_its_boundary() {
    let h = harness();
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let limit = 256 * 1024;
    let overhead = json!({"org": "acme", "name": "BIG", "value": ""})
        .to_string()
        .len();
    let body =
        |len: usize| json!({"org": "acme", "name": "BIG", "value": "a".repeat(len - overhead)});
    assert_eq!(body(limit + 1).to_string().len(), limit + 1);
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(body(limit + 1)), &token)
        .await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{b}");
    assert!(h.step_up.requests().is_empty());
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(body(limit)), &token)
        .await;
    assert!(is_step_up(st, &b), "{st} {b}");
    assert_eq!(h.step_up.requests().len(), 1);
}

/// ATTACK: the per-op cap bounds only the BODY; the raw query string and
/// path are parked on the request row too (in the clear, not sealed). Try
/// to ride a huge query past the cap. SURVIVES (bounded): the `http` crate
/// refuses any URI over 65 534 bytes before a request exists, so the most a
/// row can carry beyond the cap is ~64 KiB of query/path.
#[tokio::test]
async fn rt_a_huge_query_string_is_bounded_by_the_uri_limit() {
    assert!(
        "/v1/secrets?pad="
            .to_string()
            .add_pad(1024 * 1024)
            .parse::<axum::http::Uri>()
            .is_err(),
        "a 1 MiB URI is not even a Uri"
    );
    let h = harness();
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let uri = "/v1/secrets?pad=".to_string().add_pad(65_000);
    let (st, b) = h
        .bearer("POST", &uri, Some(secret("acme", "X")), &token)
        .await;
    assert!(is_step_up(st, &b), "{st} {b}");
    let parked = h
        .step_up
        .requests()
        .iter()
        .map(|r| r.query.len() + r.path.len())
        .max()
        .unwrap();
    assert!(parked <= 65_534, "{parked}");
}

trait AddPad {
    fn add_pad(self, n: usize) -> String;
}
impl AddPad for String {
    fn add_pad(mut self, n: usize) -> String {
        self.push_str(&"a".repeat(n));
        self
    }
}

// ---------------------------------------------------------------------------
// e582326 / 381a952 / b11e4b4 — the refusal limiter and refusal evidence
// ---------------------------------------------------------------------------

/// The refused events of `h`, oldest first.
async fn refused(h: &Harness) -> Vec<scarab_identity::SecurityEvent> {
    h.events("step_up.refused").await
}

/// ATTACK: one Org's refusals hammered past the window budget must not
/// suppress another Org's. SURVIVES (381a952 keys by Org).
#[tokio::test]
async fn rt_hammering_one_org_does_not_mute_another_orgs_refusals() {
    let h = harness();
    let (s, _soft) = amy_in_browser(&h).await;
    h.grant("amy", Scope::Org("beta".into()), Role::Owner).await;
    for i in 0..9 {
        let (st, b) = h
            .cookie(
                "POST",
                "/v1/secrets",
                Some(secret("acme", &format!("A{i}"))),
                &s,
            )
            .await;
        assert_step_up(st, &b, "window");
    }
    let (st, b) = h
        .cookie("POST", "/v1/secrets", Some(secret("beta", "B")), &s)
        .await;
    assert_step_up(st, &b, "window");
    let beta: Vec<_> = refused(&h)
        .await
        .into_iter()
        .filter(|e| e.org.as_deref() == Some("beta"))
        .collect();
    assert_eq!(beta.len(), 1, "beta's refusal is on beta's log");
}

/// ATTACK (target laundering): the limiter's key leaves the TARGET out, and
/// a closed window flushes only the NEWEST suppressed refusal. So a stolen
/// cookie session burns the budget on a decoy name, probes the real one,
/// then sends one more decoy: the log shows five decoys and a later decoy
/// "carrying 2" — the real target never appears anywhere.
#[tokio::test]
#[ignore = "redteam: BROKE the refusal limiter launders the probed target: a probe of PROD_DB_PASSWORD between decoys is never logged"]
async fn rt_the_refusal_limiter_hides_the_target_actually_probed() {
    let h = harness();
    let (s, _soft) = amy_in_browser(&h).await;
    let probe = |name: &'static str| {
        let s = s.clone();
        let h = &h;
        async move {
            let (st, b) = h
                .cookie("POST", "/v1/secrets", Some(secret("acme", name)), &s)
                .await;
            assert_step_up(st, &b, "window");
        }
    };
    for _ in 0..5 {
        probe("DECOY").await;
    }
    probe("PROD_DB_PASSWORD").await;
    probe("DECOY").await;
    h.clock.advance(MIN + 1);
    probe("DECOY").await;
    let all = refused(&h).await;
    assert!(
        all.iter().any(|e| e
            .target
            .as_deref()
            .is_some_and(|t| t.contains("PROD_DB_PASSWORD"))),
        "the probed target is nowhere in the log: {:?}",
        all.iter()
            .map(|e| (e.target.clone(), e.detail.clone()))
            .collect::<Vec<_>>()
    );
}

/// ATTACK (clock step-back): the limiter compares `now - window_start <
/// WINDOW` with no lower bound. After the wall clock steps BACK (an NTP
/// step, a VM resume), every refusal of a key that used its budget is
/// suppressed until the clock catches up with the old window — here ten
/// minutes of refusals vanish.
#[tokio::test]
#[ignore = "redteam: BROKE after a wall-clock step back the refusal limiter suppresses a key until the clock catches up (10 min here)"]
async fn rt_a_clock_step_back_mutes_refusals_until_it_catches_up() {
    let h = harness();
    let (s, _soft) = amy_in_browser(&h).await;
    for _ in 0..5 {
        h.cookie("POST", "/v1/secrets", Some(secret("acme", "X")), &s)
            .await;
    }
    let before = refused(&h).await.len();
    h.clock.advance(-10 * MIN);
    for _ in 0..3 {
        let (st, b) = h
            .cookie("POST", "/v1/secrets", Some(secret("acme", "X")), &s)
            .await;
        assert_step_up(st, &b, "window");
    }
    let after = refused(&h).await.len();
    assert!(
        after > before,
        "none of 3 refusals after a 10-minute step back was recorded"
    );
}

/// ATTACK (confused deputy, ADR-0073 §6): another Owner of the same Org
/// probes amy's pending request. b11e4b4 records it — but Org-less, as the
/// PROBER's event, and `step_up.refused` is not among the Org-less kinds an
/// Org's log shows. So the only person who can ever read the evidence is
/// the prober; the victim and every Org admin see nothing.
#[tokio::test]
async fn rt_a_request_probe_is_visible_only_to_the_prober() {
    let h = harness();
    let (amy, _soft) = amy_in_browser(&h).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret("acme", "X")), &token)
        .await;
    assert_step_up(st, &b, "window");
    let id = b["request_id"].as_str().unwrap().to_string();
    let (carl, _) = member(&h, "carl", "acme", Role::Owner).await;
    let (st, _) = h
        .cookie("GET", &format!("/v1/step-up/requests/{id}"), None, &carl)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // The prober's own log has it…
    let (_, mine) = h.cookie("GET", "/v1/me/security/events", None, &carl).await;
    assert!(
        mine["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "step_up.refused"
                && e["request_id"] == id.as_str()
                && e["detail"]["reason"] == "request_not_found"),
        "recorded for carl: {mine}"
    );
    // …the victim's and the Org's do not.
    let (_, org_log) = h
        .cookie("GET", "/v1/orgs/acme/security/events", None, &amy)
        .await;
    let (_, amys) = h.cookie("GET", "/v1/me/security/events", None, &amy).await;
    let probe_in = |page: &Value| {
        page["events"].as_array().is_some_and(|evs| {
            evs.iter().any(|e| {
                e["kind"] == "step_up.refused"
                    && e["request_id"] == id.as_str()
                    && e["detail"]["reason"] == "request_not_found"
            })
        })
    };
    assert!(
        probe_in(&org_log) || probe_in(&amys),
        "carl's probe of amy's request {id} is invisible to amy and to acme's admins"
    );
}

/// One step-up assertion with `k`, from a client that ignores the
/// challenge's allow-list hint (an attacker's client).
async fn assert_ignoring_allow_list(
    h: &Harness,
    s: &Sess,
    k: &mut SoftPasskey,
) -> (StatusCode, Value) {
    let (_, mut ch) = h.cookie("POST", "/v1/me/step-up/options", None, s).await;
    if let Some(pk) = ch["public_key"].as_object_mut() {
        pk.remove("allowCredentials");
    }
    let cred = k.assert(&ch["public_key"], ORIGIN).unwrap();
    h.cookie(
        "POST",
        "/v1/me/step-up",
        Some(json!({"challenge_id": ch["id"], "credential": cred})),
        s,
    )
    .await
}

/// ATTACK: keep using a passkey after it was flagged as a suspected clone
/// (the holder of the copy keeps trying). Only the FIRST detection writes
/// `passkey.clone_suspected`; every later assertion with the flagged key is
/// refused `clone_suspected`, which `verify_assertion` deliberately does not
/// record — so the attacker's continued use leaves no evidence at all
/// (ticket 69e06b5: "a failed assertion" writes no event).
#[tokio::test]
#[ignore = "redteam: BROKE assertions with an already clone-flagged passkey are refused but never recorded"]
async fn rt_reusing_a_clone_flagged_passkey_leaves_no_evidence() {
    let h = harness();
    let (s, mut k1, _k2, _) = amy_with_two_passkeys(&h).await;
    h.open_window(&s, &mut k1).await;
    k1.counter = Counter::Fixed(1);
    let (st, b) = assert_ignoring_allow_list(&h, &s, &mut k1).await;
    assert_eq!(
        (st, b["code"].clone()),
        (StatusCode::FORBIDDEN, json!("clone_suspected"))
    );
    let evidence = |h: &Harness| {
        let flagged = h.events_of("passkey.clone_suspected").len();
        let refused = h
            .events_of("step_up.refused")
            .into_iter()
            .filter(|e| e.detail["reason"] == "clone_suspected")
            .count();
        flagged + refused
    };
    let first = evidence(&h);
    for _ in 0..3 {
        let (st, b) = assert_ignoring_allow_list(&h, &s, &mut k1).await;
        assert_eq!(b["code"], "clone_suspected", "{st} {b}");
    }
    assert!(
        evidence(&h) > first,
        "3 more uses of the flagged key, {} evidence rows before and after",
        first
    );
}

// ---------------------------------------------------------------------------
// 6856659 / 67db99e / 8b6ee26 — passkey removal revokes exactly its own
// ---------------------------------------------------------------------------

/// ATTACK: amy (Owner, her own always-ask) deletes BOB's passkey by id.
/// SURVIVES: 404 before any revoke — bob's window stays open, his passkey
/// stays.
#[tokio::test]
async fn rt_removing_another_humans_passkey_id_revokes_nothing() {
    let h = harness();
    let (bob, mut bob_key) = member(&h, "bob", "acme", Role::Owner).await;
    let bob_pk = h.passkeys.list("bob").await.unwrap()[0].id.clone();
    h.open_window(&bob, &mut bob_key).await;
    let (st, _) = h
        .cookie("POST", "/v1/secrets", Some(secret("acme", "B1")), &bob)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (amy, mut amy_key) = amy_in_browser(&h).await;
    let uri = format!("/v1/me/passkeys/{bob_pk}");
    let (st, b) = h.cookie("DELETE", &uri, None, &amy).await;
    assert_step_up(st, &b, "action");
    h.answer(&amy, &mut amy_key, &b["challenge"]).await;
    let (st, b) = h.cookie("DELETE", &uri, None, &amy).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{b}");

    assert_eq!(h.passkeys.list("bob").await.unwrap().len(), 1);
    let (st, b) = h
        .cookie("POST", "/v1/secrets", Some(secret("acme", "B2")), &bob)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "bob's window survives: {b}");
}

/// ATTACK: removing k1 must not revoke the window k2 opened (over-revoke).
/// SURVIVES: delete_for_passkey is keyed by the passkey id.
#[tokio::test]
async fn rt_removing_one_passkey_keeps_the_other_passkeys_window() {
    let h = harness();
    let (s, _k1, mut k2, k1_id) = amy_with_two_passkeys(&h).await;
    h.open_window(&s, &mut k2).await;
    let uri = format!("/v1/me/passkeys/{k1_id}");
    let (st, b) = h.cookie("DELETE", &uri, None, &s).await;
    assert_step_up(st, &b, "action");
    h.answer(&s, &mut k2, &b["challenge"]).await;
    let (st, b) = h.cookie("DELETE", &uri, None, &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");
    let (st, b) = h
        .cookie("POST", "/v1/secrets", Some(secret("acme", "S")), &s)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "k2's window still stands: {b}");
}

/// ATTACK: remove a passkey that is already gone (a retry, a double click)
/// and one with an id that never existed, incl. an injection-shaped one.
/// SURVIVES: 404 each, nothing revoked, no `passkey.removed` recorded.
#[tokio::test]
async fn rt_removing_a_gone_or_bogus_passkey_is_a_clean_404() {
    let h = harness();
    let (s, _k1, mut k2, k1_id) = amy_with_two_passkeys(&h).await;
    h.open_window(&s, &mut k2).await;
    for id in [
        "no-such-passkey".to_string(),
        "x'%20OR%20'1'='1".to_string(),
        "%25".to_string(),
    ] {
        let uri = format!("/v1/me/passkeys/{id}");
        let (st, b) = h.cookie("DELETE", &uri, None, &s).await;
        if is_step_up(st, &b) {
            h.answer(&s, &mut k2, &b["challenge"]).await;
            let (st, b) = h.cookie("DELETE", &uri, None, &s).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{id}: {b}");
        } else {
            assert!(st.is_client_error(), "{id}: {st} {b}");
        }
    }
    assert!(h.events_of("passkey.removed").is_empty());
    let (st, _) = h
        .cookie("POST", "/v1/secrets", Some(secret("acme", "S")), &s)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "the window was not revoked");
    assert!(h
        .passkeys
        .list("amy")
        .await
        .unwrap()
        .iter()
        .any(|p| p.id == k1_id));
}

// ---------------------------------------------------------------------------
// fe777ed / e15bc4c — what an API token reads of its owner
// ---------------------------------------------------------------------------

/// ATTACK: e15bc4c hides the passkey list from a token on `/v1/me/security`
/// and refuses `/v1/me/security/events` — but an Org-scoped Admin token
/// reads the SAME passkey facts (passkey ids, labels as the target) through
/// `/v1/orgs/acme/security/events`, whose Org-less `passkey.enrolled` rows
/// about members are shown to any Org admin. "An API token does not read
/// its owner's passkeys" (the commit's claim) does not hold.
#[tokio::test]
#[ignore = "redteam: BROKE an Org Admin token reads its owner's passkey ids/labels via /v1/orgs/{org}/security/events"]
async fn rt_an_org_admin_token_reads_its_owners_passkeys_through_the_org_log() {
    let h = harness();
    let (_amy, _) = amy_in_browser(&h).await;
    let pk = h.passkeys.list("amy").await.unwrap()[0].clone();
    let token = pat(&h, "ta", Scope::Org("acme".into()), Role::Admin).await;
    let (st, me) = h.bearer("GET", "/v1/me/security", None, &token).await;
    assert_eq!(st, StatusCode::OK, "{me}");
    assert_eq!(me["passkeys"], json!([]), "hidden on /v1/me/security");
    let (st, page) = h
        .bearer("GET", "/v1/orgs/acme/security/events", None, &token)
        .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    let text = page.to_string();
    assert!(
        !text.contains(&pk.id) && !text.contains("Laptop"),
        "the token read its owner's passkey via the Org log: {page}"
    );
}

/// ATTACK: a token polls a request raised by a SIBLING token of the same
/// human. SURVIVES: only the exact requester (or the human, in a browser)
/// sees it — 404.
#[tokio::test]
async fn rt_a_sibling_token_cannot_read_another_tokens_request() {
    let h = harness();
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let t1 = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let t2 = pat(&h, "t2", Scope::Org("acme".into()), Role::Admin).await;
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret("acme", "X")), &t1)
        .await;
    assert_step_up(st, &b, "window");
    let id = b["request_id"].as_str().unwrap().to_string();
    let (st, _) = h
        .bearer("GET", &format!("/v1/step-up/requests/{id}"), None, &t2)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = h
        .bearer("GET", &format!("/v1/step-up/requests/{id}"), None, &t1)
        .await;
    assert_eq!(st, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// 51d6bcb / bfa96d3 — sessions and CSRF at rest
// ---------------------------------------------------------------------------

/// ATTACK (a database reader): take `sessions.id` and `sessions.csrf`
/// straight out of Postgres and ride them — as the cookie, as a Bearer,
/// and as the CSRF double-submit. SURVIVES: the store keys by the hash and
/// hashes what it is given, so the stored columns are not credentials.
#[tokio::test]
async fn rt_the_stored_session_columns_ride_nothing() {
    let Some((tdb, pg)) = pg().await else { return };
    let sessions = pg.clone();
    let h = harness_with(move |st| {
        st.with_auth(
            Arc::new(FakeAuthenticator::new()),
            sessions as Arc<dyn SessionStore>,
        )
    });
    let now = h.now().await;
    let real = Sess {
        id: uuid::Uuid::new_v4().to_string(),
        csrf: uuid::Uuid::new_v4().to_string(),
    };
    SessionStore::put(
        pg.as_ref(),
        &Session {
            id: real.id.clone(),
            principal: owner("amy"),
            expires_at: now + DAY,
            csrf_hash: scarab_identity::csrf_hash(&real.csrf),
            created_at: now,
            login_kind: LOGIN_KIND_BROWSER.into(),
        },
    )
    .await
    .unwrap();
    let (raw_id, raw_csrf): (String, String) = sqlx::query_as("SELECT id, csrf FROM sessions")
        .fetch_one(&tdb.pool)
        .await
        .unwrap();
    assert_ne!(raw_id, real.id);
    assert_ne!(raw_csrf, real.csrf);

    // Control: the real credentials work.
    let (st, _) = h.bearer("GET", "/v1/me", None, &real.id).await;
    assert_eq!(st, StatusCode::OK);
    // The stored id, as a Bearer and as the cookie.
    let (st, _) = h.bearer("GET", "/v1/me", None, &raw_id).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let stolen = Sess {
        id: raw_id.clone(),
        csrf: raw_csrf.clone(),
    };
    let (st, _) = h.cookie("GET", "/v1/me", None, &stolen).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // The real cookie with the STORED csrf column as the double-submit.
    let half = Sess {
        id: real.id.clone(),
        csrf: raw_csrf,
    };
    let (st, b) = h
        .cookie("POST", "/v1/secrets", Some(secret("acme", "X")), &half)
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert!(
        !is_step_up(st, &b),
        "the CSRF check refused it, not step-up: {b}"
    );
    // With the real token it gets as far as step-up.
    let mut soft = passkey();
    h.enroll(&real, &mut soft, "Laptop").await;
    let (st, b) = h
        .cookie("POST", "/v1/secrets", Some(secret("acme", "X")), &real)
        .await;
    assert_step_up(st, &b, "window");
    tdb.cleanup().await;
}

/// ATTACK: CSRF edge inputs — a session minted with NO token presented with
/// an empty header, and a real session presented with the hash of its own
/// token, an empty header, or none. SURVIVES: each mutation is refused.
#[tokio::test]
async fn rt_csrf_edge_inputs_are_refused() {
    let h = harness();
    let now = h.now().await;
    let tokenless = Session {
        id: uuid::Uuid::new_v4().to_string(),
        principal: owner("amy"),
        expires_at: now + DAY,
        csrf_hash: String::new(),
        created_at: now,
        login_kind: LOGIN_KIND_BROWSER.into(),
    };
    h.sessions.put(&tokenless).await.unwrap();
    let real = h.browser_session(owner("amy")).await;
    let cases = [
        Sess {
            id: tokenless.id.clone(),
            csrf: String::new(),
        },
        Sess {
            id: tokenless.id.clone(),
            csrf: scarab_identity::csrf_hash(""),
        },
        Sess {
            id: real.id.clone(),
            csrf: scarab_identity::csrf_hash(&real.csrf),
        },
        Sess {
            id: real.id.clone(),
            csrf: String::new(),
        },
        Sess {
            id: real.id.clone(),
            csrf: real.csrf.to_uppercase(),
        },
    ];
    for s in cases {
        let (st, b) = h
            .cookie("POST", "/v1/secrets", Some(secret("acme", "X")), &s)
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{s:?}: {b}");
        assert!(!is_step_up(st, &b), "{s:?}: {b}");
    }
    // No header at all.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/secrets")
        .header("cookie", format!("scarab_session={}", real.id))
        .header("content-type", "application/json")
        .body(Body::from(secret("acme", "X").to_string()))
        .unwrap();
    let (st, b) = h.send(req).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert!(!is_step_up(st, &b), "{b}");
}

// ---------------------------------------------------------------------------
// 97223c7 / 129166b / 8f86581 — reserved `_` orgs
// ---------------------------------------------------------------------------

/// ATTACK: reach `_plugin_connections` through `/v1/secrets` by percent-
/// encoding, case, a NUL suffix, and a repo below it; and plant a plugin in
/// `_forge` by a percent-encoded path. SURVIVES: every `_`-prefixed decoded
/// value is refused (400) and the planted key is untouched; a leading
/// space is a DIFFERENT org (no normalization anywhere), so it reaches
/// nothing reserved.
#[tokio::test]
async fn rt_reserved_org_spellings_do_not_reach_the_plugin_scope() {
    let secrets = Arc::new(FakeSecrets::new());
    let reserved = SecretScope::Org {
        org: "_plugin_connections".into(),
    };
    secrets
        .put(
            &reserved,
            Secret {
                key: "pc-1".into(),
                value: b"original".to_vec(),
            },
        )
        .await
        .unwrap();
    let app = bare_app(secrets.clone());
    let post = |body: Value| {
        Request::builder()
            .method("POST")
            .uri("/v1/secrets")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    for org in [
        "_PLUGIN_CONNECTIONS",
        "_plugin_connections\u{0}",
        "_plugin_connections",
    ] {
        let (st, b) = call(
            &app,
            post(json!({"org": org, "name": "pc-1", "value": "hijack"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{org:?}: {b}");
    }
    for uri in [
        "/v1/secrets?org=%5Fplugin_connections",
        "/v1/secrets?org=%5fplugin_connections&repo=x",
    ] {
        let (st, b) = call(&app, get(uri)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{uri}: {b}");
    }
    let del = Request::builder()
        .method("DELETE")
        .uri("/v1/secrets?org=%5Fplugin_connections&name=pc-1")
        .body(Body::empty())
        .unwrap();
    let (st, b) = call(&app, del).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    // A leading space is another org string entirely.
    let (st, b) = call(
        &app,
        post(json!({"org": " _plugin_connections", "name": "pc-1", "value": "hijack"})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");
    let kept = secrets.get(&reserved, "pc-1").await.unwrap();
    assert_eq!(kept.value, b"original".to_vec());
    // A plugin install into a percent-encoded reserved org.
    let install = Request::builder()
        .method("POST")
        .uri("/v1/orgs/%5Fforge/plugins")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"name": "x", "plugin": "argocd", "image": "img@sha256:00"}).to_string(),
        ))
        .unwrap();
    let (st, b) = call(&app, install).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
}

/// ATTACK: 97223c7 closed `/v1/secrets` over `_plugin_connections`, but the
/// secret coverage MATRIX is a second read surface over the same scope:
/// `GET /v1/repos/_plugin_connections/<any>/secrets/matrix` lists the
/// plugin connection credential keys (as Org-level rows) to an admin —
/// the very listing the ticket called an audit bypass. (The matrix's
/// silence/unsilence writes take a reserved org as well.)
#[tokio::test]
#[ignore = "redteam: BROKE the secret matrix lists _plugin_connections credential keys; the reserved-org refusal covers /v1/secrets only"]
async fn rt_the_secret_matrix_lists_the_reserved_plugin_scope() {
    let secrets = Arc::new(FakeSecrets::new());
    secrets
        .put(
            &SecretScope::Org {
                org: "_plugin_connections".into(),
            },
            Secret {
                key: "pc-0f1e2d".into(),
                value: b"bearer".to_vec(),
            },
        )
        .await
        .unwrap();
    let app = bare_app(secrets);
    let (st, b) = call(
        &app,
        get("/v1/repos/%5Fplugin_connections/anything/secrets/matrix"),
    )
    .await;
    assert!(
        st == StatusCode::BAD_REQUEST && !b.contains("pc-0f1e2d"),
        "{st}: {b}"
    );
}

/// ATTACK: bind a repo into a reserved org through the REGISTRY (the path
/// re-sync, installation sync and declarative connections take) with case
/// and length variants. SURVIVES: `ForgeConnectionStore::bind_repo` refuses
/// every `_`-prefixed org before the adapter's upsert.
#[tokio::test]
async fn rt_the_registry_refuses_every_reserved_org_spelling() {
    use scarab_forge::{ForgeConnectionStore, RegistryError, RepoRef};
    let db = InMemoryDb::new();
    let repo = RepoRef {
        owner: "_forge".into(),
        name: "web".into(),
    };
    for org in [
        "_forge",
        "_Forge",
        "_PLUGIN_CONNECTIONS",
        "_",
        "__",
        "_forge ",
    ] {
        let got = ForgeConnectionStore::bind_repo(&db, "conn-1", &repo, org, "web").await;
        assert!(
            matches!(got, Err(RegistryError::ReservedOrg(ref o)) if o == org),
            "{org:?}: {got:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 0314275 / d1fa1b3 — owner reset across identity merges
// ---------------------------------------------------------------------------

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

/// ATTACK: carl owns ONLY acme. bob (live, acme) absorbed `bobby` (an Admin
/// of beta) in a fold. carl resets by the ALIAS — hoping the alias's empty
/// membership slips past "an Owner of every Org they belong to". SURVIVES:
/// the alias is followed to bob, bob is in beta, carl does not own beta —
/// refused, beta not named, bob's passkey kept.
#[tokio::test]
async fn rt_an_alias_reset_cannot_skip_the_every_org_owner_check() {
    let Some((tdb, pg)) = pg().await else { return };
    let ids = pg.clone();
    let h = harness_on(Backend::Pg(pg.clone()), move |st| {
        st.with_identities(ids as Arc<dyn IdentityStore>)
    });
    for (provider, ext, handle) in [("github", "b1", "bob"), ("forgejo", "f1", "bobby")] {
        IdentityStore::sign_in(pg.as_ref(), &identity(provider, ext, handle), handle, T0)
            .await
            .unwrap();
    }
    h.grant("bob", Scope::Org("acme".into()), Role::Member)
        .await;
    h.grant("bobby", Scope::Org("beta".into()), Role::Admin)
        .await;
    IdentityStore::link(pg.as_ref(), "bob", &identity("forgejo", "f1", "bobby"), T0)
        .await
        .unwrap();
    let bob = h.browser_session(scoped("bob")).await;
    h.enroll(&bob, &mut passkey(), "bob's key").await;

    let (carl, mut soft) = member(&h, "carl", "acme", Role::Owner).await;
    for who in ["bobby", "bob"] {
        let uri = format!("/v1/orgs/acme/members/{who}/passkeys/reset");
        let (st, b) = h.cookie("POST", &uri, None, &carl).await;
        assert_step_up(st, &b, "action");
        h.answer(&carl, &mut soft, &b["challenge"]).await;
        let (st, b) = h.cookie("POST", &uri, None, &carl).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{who}: {b}");
        assert!(!b.to_string().contains("beta"), "{who}: {b}");
    }
    assert_eq!(h.passkey_store.list("bob").await.unwrap().len(), 1);
    assert!(h.events("passkey.reset").await.is_empty());
    tdb.cleanup().await;
}

// ---------------------------------------------------------------------------
// 584bd03 / 1708fe7 / 33a3668 — the fresh-login URL
// ---------------------------------------------------------------------------

fn provider(id: &str, unqualified: bool) -> Arc<scarab_server::oauth::OAuthAuthenticator> {
    Arc::new(scarab_server::oauth::OAuthAuthenticator::new(
        scarab_server::config::OAuthConfig {
            client_id: "cid".into(),
            client_secret: "sekret".into(),
            authorize_url: "http://127.0.0.1:9/authorize".into(),
            token_url: "http://127.0.0.1:9/token".into(),
            userinfo_url: "http://127.0.0.1:9/user".into(),
            oidc_issuer: None,
            scopes: String::new(),
            owners: vec![],
            provider_id: id.into(),
            unqualified_ids: unqualified,
            name: id.into(),
            forge_url: None,
        },
    ))
}

/// ATTACK: a subject crafted to inject into the fresh-login URL (`&`, `#`,
/// `//host`, an unconfigured `provider:` prefix, a configured prefix with a
/// payload after it). SURVIVES: only a configured provider id is ever named
/// (percent-encoded), the URL stays relative, and `next` is fixed.
#[tokio::test]
async fn rt_a_hostile_subject_cannot_steer_the_fresh_login_url() {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let st = AppState::new(db, Arc::new(FakeClock::new(0)), logs)
        .with_oauth_login(provider("github", true))
        .with_oauth_login(provider("forgejo", false));
    for subject in [
        "x&provider=evil",
        "evil:x",
        "evil&next=//attacker.example:x",
        "forgejo:x&next=//attacker.example",
        "forgejo:x#frag",
        "//attacker.example",
        "",
    ] {
        let url = st.fresh_login_url(subject).await;
        assert!(
            url.starts_with(scarab_server::FRESH_LOGIN_URL),
            "{subject:?}: {url}"
        );
        let tail = &url[scarab_server::FRESH_LOGIN_URL.len()..];
        assert!(
            tail.is_empty() || tail == "&provider=github" || tail == "&provider=forgejo",
            "{subject:?}: {url}"
        );
        assert!(!url.contains("attacker"), "{subject:?}: {url}");
    }
    assert!(st
        .fresh_login_url("forgejo:x")
        .await
        .ends_with("&provider=forgejo"));
}
