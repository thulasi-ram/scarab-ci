//! An org's Fork Run approval setting (ADR-0086 §9):
//! `GET` and `PUT /v1/orgs/{org}/security/fork-run-approval` through the
//! real router and step-up guard, over the in-memory stores, with a
//! software passkey (the authenticator is the true external).

mod step_up_harness;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{json, Value};

use scarab_identity::{
    ApiTokenStore, ForkRunApproval, ForkRunApprovalStore, IdentityError, LoginKind, PrincipalToken,
    PrincipalTokenKind, Role, Scope,
};
use scarab_testkit::InMemoryForkRunApproval;
use scarab_webauthn::soft::SoftPasskey;

use step_up_harness::*;

const URI: &str = "/v1/orgs/acme/security/fork-run-approval";
const CHANGED: &str = "fork_run_approval.changed";

fn world(store: Arc<dyn ForkRunApprovalStore>) -> Harness {
    harness_with(move |st| st.with_fork_run_approval(store))
}

/// `subject` (no global role) in a browser with a passkey, `role` in `acme`.
async fn member(h: &Harness, subject: &str, role: Role) -> (Sess, SoftPasskey) {
    h.grant(subject, Scope::Org("acme".into()), role).await;
    let s = h.browser_session(scoped(subject)).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, &format!("{subject}'s key")).await;
    (s, soft)
}

async fn put(h: &Harness, s: &Sess, uri: &str, setting: &str) -> (StatusCode, Value) {
    h.cookie("PUT", uri, Some(json!({ "setting": setting })), s)
        .await
}

/// An admin of the org changes the setting from a browser. A member reads
/// it (`can_change` false) and may not change it. A change that gates more
/// Runs needs no confirmation; one that gates fewer is asked for one that
/// names the old and the new value, and nothing is stored until it is
/// given; a request that changes nothing records nothing. Each change is
/// recorded with the old and the new value. A change made under `Acme` is
/// the setting `acme` reads.
/// Kills: a loosening with no step-up; a step-up on tightening (an admin
/// locking down during an incident is slowed); an event without the old
/// value; a change keyed by the org's spelling.
#[tokio::test]
async fn an_admin_changes_the_setting_from_a_browser() {
    let store = Arc::new(InMemoryForkRunApproval::new());
    let h = world(store.clone());
    let (dan, mut dan_key) = member(&h, "dan", Role::Admin).await;
    let (bob, _) = member(&h, "bob", Role::Member).await;

    let (st, body) = h.cookie("GET", URI, None, &dan).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "setting": "agent_started", "can_change": true })
    );
    let (st, body) = h.cookie("GET", URI, None, &bob).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["can_change"], false, "{body}");

    let (st, body) = put(&h, &bob, URI, "off").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_ne!(body["code"], "step_up_required", "{body}");
    assert_eq!(store.fork_run_approval("acme").await.unwrap(), None);

    // Tighter: no confirmation.
    let (st, body) = put(&h, &dan, URI, "all").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["setting"], "all", "{body}");
    let events = h.events(CHANGED).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].subject, "dan");
    assert_eq!(events[0].org.as_deref(), Some("acme"));
    assert_eq!(events[0].op.as_deref(), Some("fork_run_approval.put"));
    assert_eq!(events[0].confirmation_id, None);
    assert_eq!(
        events[0].detail,
        json!({ "old": "agent_started", "new": "all" })
    );

    // No change: nothing recorded.
    let (st, body) = put(&h, &dan, URI, "all").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(h.events(CHANGED).await.len(), 1);

    // Looser: asked, and nothing stored until confirmed.
    let (st, body) = put(&h, &dan, URI, "off").await;
    assert_step_up(st, &body, "action");
    assert_eq!(body["op"], "fork_run_approval.put", "{body}");
    assert_eq!(
        body["summary"]["changes"],
        json!([
            "setting = all → off",
            "fewer Runs of fork code wait for approval"
        ])
    );
    assert_eq!(
        store.fork_run_approval("acme").await.unwrap(),
        Some(ForkRunApproval::All)
    );
    let (st, conf) = h.answer(&dan, &mut dan_key, &body["challenge"]).await;
    assert_eq!(st, StatusCode::OK, "{conf}");
    let (st, body) = put(&h, &dan, URI, "off").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["setting"], "off", "{body}");
    let events = h.events(CHANGED).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].detail, json!({ "old": "all", "new": "off" }));
    assert!(events[1].confirmation_id.is_some(), "linked to the step-up");

    // Another spelling of the org is the same setting.
    let amy = h.browser_session(owner("amy")).await;
    let (st, body) = put(&h, &amy, "/v1/orgs/Acme/security/fork-run-approval", "all").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = h.cookie("GET", URI, None, &dan).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["setting"], "all", "{body}");
}

/// An API token of amy's at `acme`; returns its plaintext.
async fn pat(h: &Harness) -> String {
    let plaintext = "scarab_pat_t1_0123456789abcdef0123456789abcdef".to_string();
    let now = h.now().await;
    let record = PrincipalToken {
        id: "t1".into(),
        name: "ci-t1".into(),
        owner_subject: "amy".into(),
        scope: Scope::Org("acme".into()),
        role: Role::Admin,
        expires_at: now + 30 * 24 * 60 * 60 * 1000,
        created_by: "amy".into(),
        created_at: now,
        last_used_at: None,
        use_count: 0,
        revoked_at: None,
        kind: PrincipalTokenKind::Issued,
        operations: None,
        audience: None,
        oauth_client: None,
    };
    h.tokens
        .put(&record, &record.stored_hash(&plaintext))
        .await
        .unwrap();
    plaintext
}

/// An API token and a session sent as Bearer may not change the setting,
/// not even to a tighter value or to the value it has, so it is never part
/// of a delegated grant.
/// Kills: a token that loosens the setting for the agent it serves.
#[tokio::test]
async fn a_token_or_a_bearer_session_may_not_change_it() {
    let store = Arc::new(InMemoryForkRunApproval::new());
    let h = world(store.clone());
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let token = pat(&h).await;
    let cli = h.session(scoped("amy"), LoginKind::Api).await;
    for bearer in [token.as_str(), cli.id.as_str()] {
        for setting in ["off", "agent_started", "all"] {
            let (st, body) = h
                .bearer("PUT", URI, Some(json!({ "setting": setting })), bearer)
                .await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{setting}: {body}");
            assert_eq!(body["code"], "browser_session_required", "{body}");
        }
    }
    assert_eq!(store.fork_run_approval("acme").await.unwrap(), None);
    assert!(h.events(CHANGED).await.is_empty());
}

/// Another admin's change lands between this request's read and its write.
struct AnotherAdminWins {
    inner: InMemoryForkRunApproval,
    raced: AtomicBool,
}

#[async_trait::async_trait]
impl ForkRunApprovalStore for AnotherAdminWins {
    async fn fork_run_approval(&self, org: &str) -> Result<Option<ForkRunApproval>, IdentityError> {
        let read = self.inner.fork_run_approval(org).await?;
        if !self.raced.swap(true, Ordering::SeqCst) {
            let current = read.unwrap_or_default();
            assert!(
                self.inner
                    .put_fork_run_approval(org, current, ForkRunApproval::Off, "carol", 0)
                    .await?
            );
        }
        Ok(read)
    }

    async fn put_fork_run_approval(
        &self,
        org: &str,
        expected: ForkRunApproval,
        new: ForkRunApproval,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<bool, IdentityError> {
        self.inner
            .put_fork_run_approval(org, expected, new, updated_by, updated_at)
            .await
    }
}

/// A change decided against a value another admin has replaced since is
/// refused (409) and writes nothing: carol's `off` stays, and no change is
/// recorded for dan.
/// Kills: a blind write that overwrites the other admin's change with a
/// decision (and a step-up exemption) made against the old value.
#[tokio::test]
async fn a_change_that_lost_a_race_writes_nothing() {
    let store = Arc::new(AnotherAdminWins {
        inner: InMemoryForkRunApproval::new(),
        raced: AtomicBool::new(false),
    });
    let h = world(store.clone());
    let (dan, _) = member(&h, "dan", Role::Admin).await;

    let (st, body) = put(&h, &dan, URI, "all").await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        store.inner.fork_run_approval("acme").await.unwrap(),
        Some(ForkRunApproval::Off)
    );
    assert!(h.events(CHANGED).await.is_empty());
}
