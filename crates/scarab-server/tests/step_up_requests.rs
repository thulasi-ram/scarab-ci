//! ADR-0073 §6 (C5/C6): StepUpConfirmationRequests. A crown jewel called
//! with an API token or a session presented as Bearer is parked for the
//! human it acts for; only that human, in their own browser, with their own
//! passkey, can confirm it; it then runs once, exactly as raised, with the
//! narrower of the requester's scope and the human's live role.
//!
//! The scenarios that hinge on the store's atomicity (the pending →
//! executing compare-and-swap, the sealed body cleared on settling) run on
//! the in-memory stores AND on a real Postgres (`fresh_db`; skipped without
//! `SCARAB_TEST_DATABASE_URL`). The secret store is the one wrapped
//! collaborator: it counts writes and can hold one at its door, which is how
//! the concurrent-confirm test constructs its interleaving instead of
//! hoping the scheduler produces it.

mod common;
mod step_up_harness;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::StatusCode;
use base64::Engine;
use serde_json::{json, Value};
use tokio::sync::{Notify, Semaphore};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{
    api_token_hash, ApiToken, ApiTokenStore, Principal, RequestState, Role, Scope, SessionStore,
    StepUpRequest, StepUpRequestPendingCaps, StepUpRequestRaiseOutcome, StepUpRequester,
    LOGIN_KIND_API,
};
use scarab_secrets::{Sealer, Secret, SecretError, SecretProvider, SecretScope};
use scarab_testkit::FakeSecrets;
use scarab_webauthn::soft::SoftPasskey;

use step_up_harness::*;

const MIN: i64 = 60 * 1000;
const DAY: i64 = 24 * 60 * MIN;

// ---------------------------------------------------------------------------
// The secret store: counts writes, can hold one at the door.
// ---------------------------------------------------------------------------

struct Secrets {
    inner: FakeSecrets,
    puts: AtomicUsize,
    hold: AtomicBool,
    entered: Notify,
    release: Semaphore,
}

impl Secrets {
    fn new() -> Self {
        Self {
            inner: FakeSecrets::new(),
            puts: AtomicUsize::new(0),
            hold: AtomicBool::new(false),
            entered: Notify::new(),
            release: Semaphore::new(0),
        }
    }

    fn puts(&self) -> usize {
        self.puts.load(Ordering::SeqCst)
    }

    async fn value(&self) -> Option<String> {
        self.inner
            .get(&SecretScope::Org { org: "acme".into() }, "DB_PASSWORD")
            .await
            .ok()
            .map(|s| String::from_utf8(s.value).unwrap())
    }
}

#[async_trait]
impl SecretProvider for Secrets {
    async fn get(&self, scope: &SecretScope, key: &str) -> Result<Secret, SecretError> {
        self.inner.get(scope, key).await
    }
    async fn put(&self, scope: &SecretScope, secret: Secret) -> Result<(), SecretError> {
        if self.hold.load(Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
        }
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put(scope, secret).await
    }
    async fn list_scoped(&self, scope: &SecretScope) -> Result<Vec<String>, SecretError> {
        self.inner.list_scoped(scope).await
    }
    async fn delete(&self, scope: &SecretScope, key: &str) -> Result<(), SecretError> {
        self.inner.delete(scope, key).await
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// amy is an Owner of `acme`; the app writes secrets to [`Secrets`].
async fn world(backend: Backend) -> (Harness, Arc<Secrets>) {
    let secrets = Arc::new(Secrets::new());
    let wired = secrets.clone();
    let h = harness_on(backend, move |st| st.with_secrets(wired));
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    (h, secrets)
}

async fn pg() -> Option<(common::TestDb, Arc<PostgresDb>)> {
    let tdb = fresh_db().await?;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    Some((tdb, pg))
}

/// amy in her browser, with a passkey.
async fn amy_in_browser(h: &Harness) -> (Sess, SoftPasskey) {
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (s, soft)
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

fn secret(value: &str) -> Value {
    json!({"org": "acme", "name": "DB_PASSWORD", "value": value})
}

/// A Bearer secret write that raises a request; its id.
async fn raise(h: &Harness, bearer: &str, value: &str) -> String {
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret(value)), bearer)
        .await;
    assert_step_up(st, &b, "window");
    assert!(b.get("challenge").is_none(), "no browser ceremony: {b}");
    let id = b["request_id"].as_str().expect("a request id").to_string();
    assert_eq!(
        b["confirmation_url"],
        format!("{PUBLIC_URL}/security/confirm/{id}"),
        "{b}"
    );
    assert!(b["expires_at"].as_i64().unwrap() > h.now().await);
    id
}

async fn options(h: &Harness, s: &Sess, id: &str) -> (StatusCode, Value) {
    h.cookie(
        "POST",
        &format!("/v1/step-up/requests/{id}/options"),
        None,
        s,
    )
    .await
}

fn confirm_req(
    s: &Sess,
    soft: &mut SoftPasskey,
    id: &str,
    challenge: &Value,
) -> axum::http::Request<axum::body::Body> {
    let cred = soft.assert(&challenge["public_key"], ORIGIN).unwrap();
    cookie_req(
        "POST",
        &format!("/v1/step-up/requests/{id}/confirm"),
        Some(json!({ "challenge_id": challenge["id"], "credential": cred })),
        s,
    )
}

async fn confirm_with(
    h: &Harness,
    s: &Sess,
    soft: &mut SoftPasskey,
    id: &str,
    challenge: &Value,
) -> (StatusCode, Value) {
    h.send(confirm_req(s, soft, id, challenge)).await
}

/// Mint a challenge for request `id` and answer it.
async fn confirm(h: &Harness, s: &Sess, soft: &mut SoftPasskey, id: &str) -> (StatusCode, Value) {
    let (st, ch) = options(h, s, id).await;
    assert_eq!(st, StatusCode::OK, "{ch}");
    confirm_with(h, s, soft, id, &ch).await
}

async fn all_events(h: &Harness) -> Vec<scarab_identity::SecurityEvent> {
    h.log
        .list(&scarab_identity::SecurityEventFilter {
            limit: 10_000,
            ..Default::default()
        })
        .await
        .unwrap()
}

async fn stored(h: &Harness, id: &str) -> StepUpRequest {
    h.requests.get_request(id).await.unwrap().expect("stored")
}

// ---------------------------------------------------------------------------
// Raise, see, confirm, run once
// ---------------------------------------------------------------------------

/// The whole path: a PAT raises a request; another Owner learns nothing;
/// the token's human confirms; the secret is written exactly once; a second
/// confirm is 409; who asked and who confirmed are recorded; the sealed body
/// is gone.
async fn a_token_s_request_runs_once_for_its_human_only(backend: Backend) {
    let (h, secrets) = world(backend).await;
    let (amy, mut soft) = amy_in_browser(&h).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let id = raise(&h, &token, "S3CR3T-v1").await;
    assert_eq!(secrets.puts(), 0, "nothing ran on the raise");

    // Stored: pending, sealed (the value is not readable at rest), redacted.
    let row = stored(&h, &id).await;
    assert_eq!(row.state, RequestState::Pending);
    assert_eq!(row.for_human, "amy");
    assert_eq!(row.requester, StepUpRequester::ApiToken { id: "t1".into() });
    assert_eq!(row.org.as_deref(), Some("acme"));
    let sealed = row.sealed.clone().expect("sealed while pending");
    assert!(!String::from_utf8_lossy(&sealed).contains("S3CR3T"));
    assert!(
        !row.summary.to_string().contains("S3CR3T"),
        "{}",
        row.summary
    );

    // Another Owner of the same Org, with a passkey, in a browser: 404 on
    // everything — not even the request's existence leaks.
    h.grant("carl", Scope::Org("acme".into()), Role::Owner)
        .await;
    let carl = h.browser_session(owner("carl")).await;
    let mut carl_key = passkey();
    h.enroll(&carl, &mut carl_key, "Carl's key").await;
    let (st, _) = h
        .cookie("GET", &format!("/v1/step-up/requests/{id}"), None, &carl)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = options(&h, &carl, &id).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (_, carl_window) = h
        .cookie("POST", "/v1/me/step-up/options", None, &carl)
        .await;
    let (st, _) = confirm_with(&h, &carl, &mut carl_key, &id, &carl_window).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // The requester may poll its state.
    let (st, polled) = h
        .bearer("GET", &format!("/v1/step-up/requests/{id}"), None, &token)
        .await;
    assert_eq!(st, StatusCode::OK, "{polled}");
    assert_eq!(polled["state"], "pending");

    // The human sees what would run.
    let (st, seen) = h
        .cookie("GET", &format!("/v1/step-up/requests/{id}"), None, &amy)
        .await;
    assert_eq!(st, StatusCode::OK, "{seen}");
    assert_eq!(seen["op"], "secret.put");
    assert_eq!(seen["summary"]["operation"], "Write a secret");
    assert_eq!(seen["summary"]["target"], "acme/DB_PASSWORD");
    assert!(seen["summary"]["changes"]
        .as_array()
        .unwrap()
        .contains(&json!("value = (hidden)")));
    assert!(seen["requested_by"].as_str().unwrap().contains("ci-t1"));
    assert_eq!(seen["requested_via"], "api_token");
    assert!(!seen.to_string().contains("S3CR3T"));

    // Two challenges minted now: the second is used after the first ran.
    let (_, ch1) = options(&h, &amy, &id).await;
    let (_, ch2) = options(&h, &amy, &id).await;
    let (st, done) = confirm_with(&h, &amy, &mut soft, &id, &ch1).await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "executed", "{done}");
    assert_eq!(done["result_status"], 204);
    assert_eq!(secrets.puts(), 1);
    assert_eq!(secrets.value().await.as_deref(), Some("S3CR3T-v1"));

    let (st, again) = confirm_with(&h, &amy, &mut soft, &id, &ch2).await;
    assert_eq!(st, StatusCode::CONFLICT, "{again}");
    let (st, _) = options(&h, &amy, &id).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(secrets.puts(), 1, "it ran exactly once");

    // The record: settled, who confirmed, no body.
    let row = stored(&h, &id).await;
    assert_eq!(row.state, RequestState::Executed);
    assert_eq!(row.confirmed_by.as_deref(), Some("amy"));
    assert_eq!(row.result_status, Some(204));
    assert!(
        row.sealed.is_none(),
        "the sealed body is cleared when it settles"
    );
    let confirmation_id = row.confirmation_id.clone().expect("linked");

    // The log: raised (by the token) → confirmed (passkey, session) → the
    // operation, authorised by the request → executed.
    let raised = h.events("request.raised").await;
    assert_eq!(raised.len(), 1);
    assert_eq!(raised[0].request_id.as_deref(), Some(id.as_str()));
    assert_eq!(raised[0].caller, "api_token:t1");
    assert_eq!(raised[0].subject, "amy");
    assert_eq!(raised[0].org.as_deref(), Some("acme"));
    let refused = h.events("step_up.refused").await;
    assert!(refused
        .iter()
        .any(|e| e.request_id.as_deref() == Some(id.as_str()) && e.caller == "api_token:t1"));
    let confirmed: Vec<_> = h
        .events("step_up.confirmed")
        .await
        .into_iter()
        .filter(|e| e.request_id.as_deref() == Some(id.as_str()))
        .collect();
    assert_eq!(confirmed.len(), 1);
    assert_eq!(
        confirmed[0].confirmation_id.as_deref(),
        Some(confirmation_id.as_str())
    );
    assert_eq!(confirmed[0].detail["kind"], "request");
    assert!(confirmed[0].passkey_id.is_some() && confirmed[0].session_hash.is_some());
    assert_eq!(confirmed[0].caller, "session:cookie");
    let req_confirmed = h.events("request.confirmed").await;
    assert_eq!(req_confirmed.len(), 1);
    assert_eq!(req_confirmed[0].detail["confirmed_by"], "amy");
    assert!(req_confirmed[0].detail["requested_by"]
        .as_str()
        .unwrap()
        .contains("ci-t1"));
    for kind in ["op.authorised", "op.completed"] {
        let ops: Vec<_> = h
            .events(kind)
            .await
            .into_iter()
            .filter(|e| e.request_id.as_deref() == Some(id.as_str()))
            .collect();
        assert_eq!(ops.len(), 1, "{kind}");
        assert_eq!(ops[0].op.as_deref(), Some("secret.put"));
        assert_eq!(
            ops[0].confirmation_id.as_deref(),
            Some(confirmation_id.as_str())
        );
        assert_eq!(
            ops[0].caller, "api_token:t1",
            "the operation ran as the token"
        );
    }
    let executed = h.events("request.executed").await;
    assert_eq!(executed.len(), 1);
    assert_eq!(executed[0].detail["status"], 204);
    assert!(h.events("request.failed").await.is_empty());
    let log = format!("{:?}", all_events(&h).await);
    assert!(!log.contains("S3CR3T"), "an event recorded the value");
}

#[tokio::test]
async fn a_token_s_request_runs_once_for_its_human_only_in_memory() {
    a_token_s_request_runs_once_for_its_human_only(Backend::Memory).await;
}

#[tokio::test]
async fn a_token_s_request_runs_once_for_its_human_only_on_postgres() {
    let Some((tdb, pg)) = pg().await else { return };
    a_token_s_request_runs_once_for_its_human_only(Backend::Pg(pg)).await;
    tdb.cleanup().await;
}

/// Two confirms of one request: the second arrives while the first is
/// INSIDE its replay (held at the secret store's door — constructed, not
/// scheduled). Exactly one executes.
async fn two_concurrent_confirms_run_it_once(backend: Backend) {
    let (h, secrets) = world(backend).await;
    let (amy, mut soft) = amy_in_browser(&h).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let id = raise(&h, &token, "v1").await;
    let (_, ch_a) = options(&h, &amy, &id).await;
    let (_, ch_b) = options(&h, &amy, &id).await;
    let req_a = confirm_req(&amy, &mut soft, &id, &ch_a);
    let req_b = confirm_req(&amy, &mut soft, &id, &ch_b);

    secrets.hold.store(true, Ordering::SeqCst);
    let app = h.app.clone();
    let first = tokio::spawn(async move { app.oneshot(req_a).await.unwrap() });
    secrets.entered.notified().await;
    // A is executing, mid-write. B:
    let (st, b) = h.send(req_b).await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    secrets.release.add_permits(1);
    let resp = first.await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["state"], "executed", "{body}");
    assert_eq!(secrets.puts(), 1, "exactly one confirm executed");
    assert_eq!(h.events("request.executed").await.len(), 1);
}

#[tokio::test]
async fn two_concurrent_confirms_run_it_once_in_memory() {
    two_concurrent_confirms_run_it_once(Backend::Memory).await;
}

#[tokio::test]
async fn two_concurrent_confirms_run_it_once_on_postgres() {
    let Some((tdb, pg)) = pg().await else { return };
    two_concurrent_confirms_run_it_once(Backend::Pg(pg)).await;
    tdb.cleanup().await;
}

/// Five minutes after it was raised, a request is gone: 410, and it never
/// runs.
async fn an_expired_request_is_gone(backend: Backend) {
    let (h, secrets) = world(backend).await;
    let (amy, mut soft) = amy_in_browser(&h).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let id = raise(&h, &token, "v1").await;
    h.clock.advance(4 * MIN);
    let (st, ch) = options(&h, &amy, &id).await;
    assert_eq!(st, StatusCode::OK, "{ch}");
    h.clock.advance(MIN);
    let (_, seen) = h
        .cookie("GET", &format!("/v1/step-up/requests/{id}"), None, &amy)
        .await;
    assert_eq!(seen["state"], "expired");
    let (st, b) = confirm_with(&h, &amy, &mut soft, &id, &ch).await;
    assert_eq!(st, StatusCode::GONE, "{b}");
    let (st, _) = options(&h, &amy, &id).await;
    assert_eq!(st, StatusCode::GONE);
    assert_eq!(secrets.puts(), 0);
    assert_eq!(stored(&h, &id).await.state, RequestState::Pending);
}

#[tokio::test]
async fn an_expired_request_is_gone_in_memory() {
    an_expired_request_is_gone(Backend::Memory).await;
}

#[tokio::test]
async fn an_expired_request_is_gone_on_postgres() {
    let Some((tdb, pg)) = pg().await else { return };
    an_expired_request_is_gone(Backend::Pg(pg)).await;
    tdb.cleanup().await;
}

// ---------------------------------------------------------------------------
// The narrower of the requester's scope and the human's live role
// ---------------------------------------------------------------------------

/// A token whose ceiling is Member never even raises a request for an
/// Administer operation (its effective role cannot reach the operation, so
/// the handler refuses it for its role) — and a token scoped to one Project
/// that raises one for an Org-level secret has it confirmed by an Owner
/// human and still FAILS at replay: the token's scope narrows it.
#[tokio::test]
async fn the_token_s_scope_and_ceiling_narrow_the_replay() {
    let (h, secrets) = world(Backend::Memory).await;
    let (amy, mut soft) = amy_in_browser(&h).await;

    let member = pat(&h, "t-member", Scope::Org("acme".into()), Role::Member).await;
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret("v")), &member)
        .await;
    assert_eq!(
        (st, b),
        (
            StatusCode::FORBIDDEN,
            Value::String("insufficient role".into())
        )
    );
    assert!(h.step_up.requests().is_empty(), "no request for it");

    let project = pat(
        &h,
        "t-project",
        Scope::Project {
            org: "acme".into(),
            name: "web".into(),
        },
        Role::Admin,
    )
    .await;
    let id = raise(&h, &project, "v").await;
    let (st, done) = confirm(&h, &amy, &mut soft, &id).await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "failed", "{done}");
    assert_eq!(done["result_status"], 403);
    assert_eq!(secrets.puts(), 0);
    let row = stored(&h, &id).await;
    assert_eq!(row.state, RequestState::Failed);
    assert!(row.sealed.is_none());
    assert_eq!(h.events("request.failed").await.len(), 1);
}

/// The human is demoted between the raise and the confirm: the replay runs
/// with the role they hold NOW, and fails.
#[tokio::test]
async fn a_human_demoted_before_the_confirm_fails_it() {
    let (h, secrets) = world(Backend::Memory).await;
    let (amy, mut soft) = amy_in_browser(&h).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let id = raise(&h, &token, "v").await;
    h.grant("amy", Scope::Org("acme".into()), Role::Member)
        .await;
    let (st, done) = confirm(&h, &amy, &mut soft, &id).await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "failed");
    assert_eq!(done["result_status"], 403);
    assert_eq!(secrets.puts(), 0);
}

/// The token is revoked before the confirm: it asks for nothing any more.
#[tokio::test]
async fn a_token_revoked_before_the_confirm_fails_it() {
    let (h, secrets) = world(Backend::Memory).await;
    let (amy, mut soft) = amy_in_browser(&h).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let id = raise(&h, &token, "v").await;
    let now = h.now().await;
    assert!(h.tokens.revoke("t1", now).await.unwrap());
    let (st, done) = confirm(&h, &amy, &mut soft, &id).await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "failed");
    assert_eq!(done["result_status"], 401);
    assert_eq!(secrets.puts(), 0);
}

/// A PAT whose ceiling is Owner, of a human holding an open window in their
/// browser, is still never stepped up: it gets the request link, and nothing
/// runs until the human confirms that exact request.
#[tokio::test]
async fn an_owner_token_is_never_satisfied_by_its_human_s_window() {
    let (h, secrets) = world(Backend::Memory).await;
    let (amy, mut soft) = amy_in_browser(&h).await;
    h.open_window(&amy, &mut soft).await;
    let token = pat(&h, "t-owner", Scope::Org("acme".into()), Role::Owner).await;
    for v in ["a", "b"] {
        raise(&h, &token, v).await;
    }
    assert_eq!(secrets.puts(), 0);
    // The human's own browser, meanwhile, is inside its window.
    let (st, _) = h
        .cookie("POST", "/v1/secrets", Some(secret("c")), &amy)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// Bearer sessions, plugins, the pending cap, widening
// ---------------------------------------------------------------------------

/// A CLI session (Bearer) raises a request its human confirms in the
/// browser; a CLI session that logged out in between runs nothing.
#[tokio::test]
async fn a_bearer_session_s_request_runs_and_a_logged_out_one_does_not() {
    let (h, secrets) = world(Backend::Memory).await;
    let (amy, mut soft) = amy_in_browser(&h).await;
    let cli = h.session(owner("amy"), LOGIN_KIND_API).await;
    let id = raise(&h, &cli.id, "from-the-cli").await;
    let row = stored(&h, &id).await;
    assert!(matches!(
        row.requester,
        StepUpRequester::BearerSession { .. }
    ));
    assert_eq!(row.session_role_cap, Some(Role::Owner));
    let (st, polled) = h
        .bearer("GET", &format!("/v1/step-up/requests/{id}"), None, &cli.id)
        .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the CLI polls its own request: {polled}"
    );
    assert_eq!(polled["requested_via"], "bearer_session");
    let (st, done) = confirm(&h, &amy, &mut soft, &id).await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "executed", "{done}");
    assert_eq!(secrets.value().await.as_deref(), Some("from-the-cli"));
    let ops: Vec<_> = h
        .events("op.authorised")
        .await
        .into_iter()
        .filter(|e| e.request_id.as_deref() == Some(id.as_str()))
        .collect();
    assert_eq!(ops[0].caller, "session:bearer");

    let id = raise(&h, &cli.id, "after-logout").await;
    h.sessions.delete(&cli.id).await.unwrap();
    let (st, done) = confirm(&h, &amy, &mut soft, &id).await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "failed");
    assert_eq!(done["result_status"], 401);
    assert_eq!(secrets.value().await.as_deref(), Some("from-the-cli"));
}

/// A PluginPrincipal has no human: it is refused, and no request exists.
#[tokio::test]
async fn a_plugin_subject_never_raises_a_request() {
    let (h, _) = world(Backend::Memory).await;
    h.grant("plugin:argo", Scope::Org("acme".into()), Role::Admin)
        .await;
    let s = h
        .session(
            Principal {
                subject: "plugin:argo".into(),
                display_name: None,
                roles: vec![],
            },
            LOGIN_KIND_API,
        )
        .await;
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret("v")), &s.id)
        .await;
    assert_step_up(st, &b, "window");
    assert!(b.get("confirmation_url").is_none(), "{b}");
    assert!(b.get("request_id").is_none(), "{b}");
    assert!(h.step_up.requests().is_empty());
    assert!(h.events("request.raised").await.is_empty());
    let refused = h.events("step_up.refused").await;
    assert_eq!(refused.last().unwrap().detail["reason"], "plugin");
}

/// At most 10 requests wait per requester; the 11th is 429 (and recorded as
/// a refusal). Another credential is not affected.
#[tokio::test]
async fn the_eleventh_pending_request_is_429() {
    let (h, _) = world(Backend::Memory).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    for i in 0..10 {
        raise(&h, &token, &format!("v{i}")).await;
    }
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret("v10")), &token)
        .await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{b}");
    assert_eq!(h.step_up.requests().len(), 10);
    let refused = h.events("step_up.refused").await;
    assert_eq!(
        refused.last().unwrap().detail["reason"],
        "too many pending confirmation requests"
    );
    let other = pat(&h, "t2", Scope::Org("acme".into()), Role::Admin).await;
    raise(&h, &other, "v").await;
    // Once they expire, the token may ask again.
    h.clock.advance(5 * MIN);
    raise(&h, &token, "later").await;
}

/// The pending cap is per human too (git-bug e582326): however many tokens
/// act for one human, at most 20 requests wait for them; the 21st is 429
/// (recorded) — and the per-credential cap still holds.
#[tokio::test]
async fn at_most_twenty_requests_wait_for_one_human_across_credentials() {
    let (h, _) = world(Backend::Memory).await;
    for t in ["t1", "t2"] {
        let token = pat(&h, t, Scope::Org("acme".into()), Role::Admin).await;
        for i in 0..10 {
            raise(&h, &token, &format!("{t}-{i}")).await;
        }
    }
    let third = pat(&h, "t3", Scope::Org("acme".into()), Role::Admin).await;
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret("t3")), &third)
        .await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{b}");
    assert_eq!(h.step_up.requests().len(), 20);
    let refused = h.events("step_up.refused").await;
    assert_eq!(
        refused.last().unwrap().detail["reason"],
        "too many pending confirmation requests for this human"
    );
    // Once they expire, the human may be asked again.
    h.clock.advance(5 * MIN);
    raise(&h, &third, "later").await;
}

/// A request seals at most its operation's body cap (git-bug e582326): a
/// 300 KiB secret raises none (413, recorded); a 100 KiB one still does
/// (secret.put allows 256 KiB), and so does a body within another op's
/// 64 KiB default.
#[tokio::test]
async fn a_body_over_the_op_s_cap_raises_no_request() {
    let (h, _) = world(Backend::Memory).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let big = "x".repeat(300 * 1024);
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret(&big)), &token)
        .await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{st}");
    assert_eq!(b["code"], "request_body_too_large", "{b}");
    assert!(h.step_up.requests().is_empty());
    let refused = h.events("step_up.refused").await;
    assert_eq!(
        refused.last().unwrap().detail["reason"],
        "request body too large"
    );
    raise(&h, &token, &"y".repeat(100 * 1024)).await;
    assert_eq!(h.step_up.requests().len(), 1);

    // binding.put keeps the 64 KiB default.
    let (st, b) = h
        .bearer(
            "PUT",
            "/v1/orgs/acme/bindings",
            Some(json!({"subject": "bob", "role": "admin", "note": "z".repeat(70 * 1024)})),
            &token,
        )
        .await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{b}");
    assert_eq!(h.step_up.requests().len(), 1);
}

/// Refusals are rate-limited per (subject, caller, operation, reason) (git-bug
/// e582326): a token hammering an operation it can never run writes 5
/// `step_up.refused` rows a minute, not one per call; the next window's
/// first row carries how many were dropped.
#[tokio::test]
async fn refusals_are_rate_limited_and_coalesced() {
    let (h, _) = world(Backend::Memory).await;
    h.grant("amy", Scope::Org("beta".into()), Role::Owner).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Owner).await;
    let beta = json!({"org": "beta", "name": "DB_PASSWORD", "value": "v"});
    let hammer = |n: usize| {
        let (h, token, beta) = (&h, &token, &beta);
        async move {
            for _ in 0..n {
                let (st, _) = h
                    .bearer("POST", "/v1/secrets", Some(beta.clone()), token)
                    .await;
                assert_eq!(st, StatusCode::FORBIDDEN);
            }
        }
    };
    hammer(12).await;
    let refused = h.events("step_up.refused").await;
    assert_eq!(refused.len(), 5, "5 a minute, not 12");
    assert!(refused
        .iter()
        .all(|e| e.detail.get("suppressed_before").is_none()));

    h.clock.advance(MIN);
    hammer(1).await;
    let refused = h.events("step_up.refused").await;
    assert_eq!(refused.len(), 6);
    assert_eq!(refused.last().unwrap().detail["suppressed_before"], 7);

    // Another reason (or caller) is its own budget: amy's browser is
    // refused and recorded however hard her token hammered.
    let amy = h.browser_session(owner("amy")).await;
    let (st, _) = h
        .cookie("POST", "/v1/secrets", Some(secret("v")), &amy)
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(h.events("step_up.refused").await.len(), 7);
}

/// git-bug 69e06b5: ceremony-level refusals are recorded (best-effort,
/// rate-limited): a token at a browser-only ceremony, an assertion that
/// fails verification, and another admin probing a request — with one
/// reason whether or not the request exists, so the prober's own log is no
/// existence oracle.
#[tokio::test]
async fn ceremony_refusals_are_recorded() {
    let (h, _) = world(Backend::Memory).await;
    let reasons = |events: Vec<scarab_identity::SecurityEvent>, subject: &str| -> Vec<String> {
        events
            .into_iter()
            .filter(|e| e.subject == subject && e.detail["mode"] == "ceremony")
            .map(|e| e.detail["reason"].as_str().unwrap().to_string())
            .collect()
    };

    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let (st, b) = h
        .bearer("POST", "/v1/me/step-up/options", None, &token)
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(
        reasons(h.events("step_up.refused").await, "amy"),
        vec!["browser_session_required"]
    );

    // An assertion made for another origin fails verification.
    let (amy, mut soft) = amy_in_browser(&h).await;
    let (st, ch) = h.cookie("POST", "/v1/me/step-up/options", None, &amy).await;
    assert_eq!(st, StatusCode::OK, "{ch}");
    let cred = soft
        .assert(&ch["public_key"], "https://evil.example")
        .unwrap();
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/me/step-up",
            Some(json!({ "challenge_id": ch["id"], "credential": cred })),
            &amy,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    let code = b["code"].as_str().unwrap().to_string();
    assert_eq!(
        reasons(h.events("step_up.refused").await, "amy"),
        vec!["browser_session_required".to_string(), code]
    );

    // Another Owner probes amy's request, and a request that never was.
    let id = raise(&h, &token, "v").await;
    h.grant("bob", Scope::Org("acme".into()), Role::Owner).await;
    let bob = h.browser_session(scoped("bob")).await;
    for probe in [id.as_str(), "no-such-request"] {
        let (st, _) = h
            .cookie("GET", &format!("/v1/step-up/requests/{probe}"), None, &bob)
            .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
    let (st, _) = options(&h, &bob, &id).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let probes: Vec<_> = h
        .events("step_up.refused")
        .await
        .into_iter()
        .filter(|e| e.subject == "bob")
        .collect();
    assert_eq!(probes.len(), 3);
    assert!(probes.iter().all(|e| e.detail
        == json!({"mode": "ceremony", "reason": "request_not_found"})
        && e.org.is_none()));
}

/// A machine caller never raises a request it could never execute
/// (git-bug ff7a014): the identity-link flows are browser-only (from a token
/// AND from a Bearer session), a token may not mint tokens, and a token
/// cannot act on an Org outside its scope. Each is a plain 403 naming why,
/// recorded as a refusal; no request is raised, so its human is never sent
/// a prompt that could only fail.
#[tokio::test]
async fn a_request_the_requester_could_never_execute_is_not_raised() {
    let (h, _) = world(Backend::Memory).await;
    h.grant("amy", Scope::Org("beta".into()), Role::Owner).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Owner).await;
    let cli = h.session(owner("amy"), LOGIN_KIND_API).await;
    let mint = json!({"name": "next", "role": "admin", "expires_in_days": 1});
    let beta_secret = json!({"org": "beta", "name": "DB_PASSWORD", "value": "v"});
    let cases: Vec<(&str, &str, Option<Value>, &str, &str)> = vec![
        (
            "POST",
            "/v1/me/identities/link",
            Some(json!({"provider": "github"})),
            &token,
            "browser_session_required",
        ),
        (
            "DELETE",
            "/v1/me/identities/github",
            None,
            &token,
            "browser_session_required",
        ),
        (
            "POST",
            "/v1/me/identities/link",
            Some(json!({"provider": "github"})),
            &cli.id,
            "browser_session_required",
        ),
        (
            "DELETE",
            "/v1/me/identities/github",
            None,
            &cli.id,
            "browser_session_required",
        ),
        (
            "POST",
            "/v1/orgs/acme/tokens",
            Some(mint.clone()),
            &token,
            "not_by_api_token",
        ),
        (
            "POST",
            "/v1/secrets",
            Some(beta_secret),
            &token,
            "outside_token_scope",
        ),
    ];
    for (method, uri, body, bearer, code) in cases {
        let (st, b) = h.bearer(method, uri, body, bearer).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{method} {uri}: {b}");
        assert_eq!(b["code"], code, "{method} {uri}: {b}");
        assert!(b.get("request_id").is_none(), "{method} {uri}: {b}");
        assert!(
            h.step_up.requests().is_empty(),
            "{method} {uri} raised a request"
        );
        let refused = h.events("step_up.refused").await;
        assert_eq!(refused.last().unwrap().detail["reason"], code, "{uri}");
    }
    assert!(h.events("request.raised").await.is_empty());

    // What the requester CAN execute still raises one: a Bearer session may
    // mint a token, and the token may write acme's secret.
    let (st, b) = h
        .bearer("POST", "/v1/orgs/acme/tokens", Some(mint), &cli.id)
        .await;
    assert_step_up(st, &b, "action");
    assert!(b["request_id"].is_string(), "{b}");
    raise(&h, &token, "v").await;
    assert_eq!(h.step_up.requests().len(), 2);
}

/// A request runs exactly what was raised: a challenge for another request
/// confirms nothing, and a stored body that no longer matches the digest
/// the human confirmed is refused at replay.
#[tokio::test]
async fn a_request_cannot_be_widened() {
    let (h, secrets) = world(Backend::Memory).await;
    let (amy, mut soft) = amy_in_browser(&h).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let first = raise(&h, &token, "first").await;
    let second = raise(&h, &token, "second").await;

    // R2's challenge, posted to R1.
    let (_, ch2) = options(&h, &amy, &second).await;
    let (st, b) = confirm_with(&h, &amy, &mut soft, &first, &ch2).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["code"], "challenge_invalid");

    // R1 runs R1.
    let (_, done) = confirm(&h, &amy, &mut soft, &first).await;
    assert_eq!(done["state"], "executed", "{done}");
    assert_eq!(secrets.value().await.as_deref(), Some("first"));

    // A request whose stored body was swapped (the digest is R2's).
    let mut forged = stored(&h, &second).await;
    forged.id = "forged".into();
    let body = serde_json::to_vec(&secret("EVIL")).unwrap();
    let plain = json!({ "body": base64::engine::general_purpose::STANDARD.encode(body) });
    // Sealed by the production sealer under the harness's KEK, so the replay
    // opens it and only the digest check stands in the way.
    forged.sealed = Some(
        real_sealer()
            .seal(plain.to_string().as_bytes(), b"step-up-request:forged")
            .unwrap(),
    );
    let uncapped = StepUpRequestPendingCaps {
        per_requester: u64::MAX,
        per_human: u64::MAX,
    };
    let raised = h
        .requests
        .raise_request(&forged, uncapped, 0)
        .await
        .unwrap();
    assert!(
        matches!(raised, StepUpRequestRaiseOutcome::Raised),
        "{raised:?}"
    );
    let (st, done) = confirm(&h, &amy, &mut soft, "forged").await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "failed", "{done}");
    assert_eq!(done["result_status"], 403);
    assert_eq!(done["result_body"]["code"], "step_up_required");
    assert_eq!(secrets.value().await.as_deref(), Some("first"));
    assert_eq!(secrets.puts(), 1);
}

/// A world with `acme/prod` governed in memory, its baseline
/// `{approvers:[amy], allowed_refs:[main]}`; and the store, to change it
/// behind the request's back.
async fn governed() -> (
    Harness,
    Arc<scarab_testkit::InMemoryEnvironmentGovernance>,
) {
    use scarab_project::{
        EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProtectionRules,
    };
    let gov = Arc::new(
        scarab_testkit::InMemoryEnvironmentGovernance::new().with_environment(
            "acme",
            "prod",
            EnvironmentProjectEligibility::AllProjects,
        ),
    );
    let wired = gov.clone();
    let h = harness_with(move |st| {
        st.with_environment_governance(wired as Arc<dyn EnvironmentGovernanceStore>)
    });
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    gov.put_environment_protection_expecting(
        "acme",
        "prod",
        &EnvironmentProtectionRules {
            approvers: vec!["amy".into()],
            allowed_refs: vec!["main".into()],
            ..Default::default()
        },
        None,
        "seed",
        0,
    )
    .await
    .unwrap();
    (h, gov)
}

async fn protection(
    gov: &scarab_testkit::InMemoryEnvironmentGovernance,
) -> scarab_project::EnvironmentProtectionRules {
    use scarab_project::EnvironmentGovernanceStore;
    gov.deploy_governance("acme", "", "prod")
        .await
        .unwrap()
        .expect("acme/prod")
        .environment
        .protection
}

/// Move the stored baseline to `rules`, as a concurrent admin would.
async fn concurrently(
    gov: &scarab_testkit::InMemoryEnvironmentGovernance,
    rules: scarab_project::EnvironmentProtectionRules,
) {
    use scarab_project::EnvironmentGovernanceStore;
    let current = protection(gov).await;
    gov.put_environment_protection_expecting("acme", "prod", &rules, Some(&current), "bob", 1)
        .await
        .unwrap();
}

/// A token raises "allow any ref" on prod's baseline; the human confirms
/// exactly that. Before the confirm, someone ADDS approver bob — so the
/// same body would now also remove bob, a loosening the human never saw.
/// The replay refuses it: the request fails and bob stays.
#[tokio::test]
async fn a_replay_cannot_apply_a_loosening_the_human_never_saw() {
    let (h, gov) = governed().await;
    let (amy, mut soft) = amy_in_browser(&h).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let uri = "/v1/orgs/acme/environments/prod/protection";
    let (st, b) = h
        .bearer(
            "PUT",
            uri,
            Some(json!({"approvers": ["amy"], "allowed_refs": []})),
            &token,
        )
        .await;
    assert!(is_step_up(st, &b), "{st} {b}");
    let id = b["request_id"].as_str().expect("a request").to_string();
    assert_eq!(b["summary"]["changes"], json!(["allows any ref"]), "{b}");

    concurrently(
        &gov,
        scarab_project::EnvironmentProtectionRules {
            approvers: vec!["amy".into(), "bob".into()],
            allowed_refs: vec!["main".into()],
            ..Default::default()
        },
    )
    .await;

    let (st, done) = confirm(&h, &amy, &mut soft, &id).await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "failed", "{done}");
    assert_eq!(stored(&h, &id).await.state, RequestState::Failed);
    let now = protection(&gov).await;
    assert!(now.approvers.contains(&"bob".to_string()), "{now:?}");
    assert_eq!(now.allowed_refs, vec!["main".to_string()]);
}

/// The control: a concurrent change that leaves the loosenings within what
/// the human confirmed (another allowed ref, still widened to "any") runs.
#[tokio::test]
async fn a_replay_within_the_confirmed_loosenings_still_runs() {
    let (h, gov) = governed().await;
    let (amy, mut soft) = amy_in_browser(&h).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let (st, b) = h
        .bearer(
            "PUT",
            "/v1/orgs/acme/environments/prod/protection",
            Some(json!({"approvers": ["amy"], "allowed_refs": []})),
            &token,
        )
        .await;
    assert!(is_step_up(st, &b), "{st} {b}");
    let id = b["request_id"].as_str().expect("a request").to_string();

    concurrently(
        &gov,
        scarab_project::EnvironmentProtectionRules {
            approvers: vec!["amy".into()],
            allowed_refs: vec!["main".into(), "release".into()],
            ..Default::default()
        },
    )
    .await;

    let (st, done) = confirm(&h, &amy, &mut soft, &id).await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "executed", "{done}");
    assert!(protection(&gov).await.allowed_refs.is_empty());
}

/// A request shows its query: `DELETE …?unbind_repos=true` cascades to the
/// connection's Projects, so its human must see the flag, not just "Delete
/// a forge connection `gh`".
#[tokio::test]
async fn a_request_shows_the_query_it_runs_with() {
    let (h, _secrets) = world(Backend::Memory).await;
    let (amy, _soft) = amy_in_browser(&h).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let (st, b) = h
        .bearer("DELETE", "/v1/connections/gh?unbind_repos=true", None, &token)
        .await;
    assert!(is_step_up(st, &b), "{st} {b}");
    let id = b["request_id"].as_str().expect("a request").to_string();
    let (st, seen) = h
        .cookie("GET", &format!("/v1/step-up/requests/{id}"), None, &amy)
        .await;
    assert_eq!(st, StatusCode::OK, "{seen}");
    assert_eq!(seen["query"], "unbind_repos=true", "{seen}");
    assert!(
        seen["summary"]["changes"]
            .as_array()
            .unwrap()
            .contains(&json!("?unbind_repos = true")),
        "{seen}"
    );
}

/// Retention: expired challenges and confirmations go, an expired request's
/// sealed body goes at once, a settled request goes after 30 days — and the
/// event log is never touched.
#[tokio::test]
async fn retention_reaps_step_up_rows_but_never_events() {
    let (h, _) = world(Backend::Memory).await;
    let (amy, mut soft) = amy_in_browser(&h).await;
    let token = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let done_id = raise(&h, &token, "v").await;
    confirm(&h, &amy, &mut soft, &done_id).await;
    let waiting = raise(&h, &token, "w").await;
    h.open_window(&amy, &mut soft).await;
    let events_before = all_events(&h).await;

    // +6 min: the waiting request expired — its body goes now, the row stays.
    h.clock.advance(6 * MIN);
    assert!(sweep(&h).await > 0);
    let row = stored(&h, &waiting).await;
    assert!(row.sealed.is_none());
    assert!(
        h.step_up.confirmations().is_empty(),
        "expired confirmations go"
    );
    assert_eq!(h.step_up.requests().len(), 2);

    // +31 days: both requests go.
    h.clock.advance(31 * DAY);
    sweep(&h).await;
    assert!(h.step_up.requests().is_empty());
    assert_eq!(
        all_events(&h).await,
        events_before,
        "the event log is never reaped"
    );
}

async fn sweep(h: &Harness) -> u64 {
    let db: Arc<dyn scarab_engine::Db> = h.db.clone();
    let clock: Arc<dyn scarab_engine::Clock> = h.clock.clone();
    scarab_server::retention::sweep_step_up(
        &db,
        h.challenge_store.as_ref(),
        h.requests.as_ref(),
        &clock,
        "replica-1",
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_member_s_own_events_and_the_request_records_stay_theirs() {
    // The requester of one token cannot poll another token's request.
    let (h, _) = world(Backend::Memory).await;
    let t1 = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let t2 = pat(&h, "t2", Scope::Org("acme".into()), Role::Admin).await;
    let id = raise(&h, &t1, "v").await;
    let (st, _) = h
        .bearer("GET", &format!("/v1/step-up/requests/{id}"), None, &t2)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // amy's own Bearer session is not her browser: no view for it either.
    let cli = h.session(owner("amy"), LOGIN_KIND_API).await;
    let (st, _) = h
        .bearer("GET", &format!("/v1/step-up/requests/{id}"), None, &cli.id)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
