//! Shared harness for the ADR-0073 step-up tests: an `AppState` with login,
//! RBAC, tokens, secrets and **enforced** step-up over the in-memory stores,
//! a `FakeClock`, and a software passkey (the one true external — the
//! authenticator) driving the real ceremony endpoints.

#![allow(dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_db_postgres::PostgresDb;
use scarab_identity::{
    Binding, BindingOrigin, ChallengeStore, PasskeyStore, Principal, RbacStore, Role, Scope,
    SecurityEvent, SecurityEventFilter, SecurityEventLog, Session, SessionStore, StepUpStore,
    LOGIN_KIND_BROWSER,
};
use scarab_server::step_up::StepUpService;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeSecrets, InMemoryApiTokens, InMemoryChallenges, InMemoryDb,
    InMemoryObjectStore, InMemoryPasskeys, InMemoryRbac, InMemorySealer, InMemorySecurityEvents,
    InMemorySessions, InMemoryStepUp,
};
use scarab_webauthn::soft::{SoftAlg, SoftPasskey};
use scarab_webauthn::RelyingParty;

/// The public URL the relying party is derived from.
pub const PUBLIC_URL: &str = "https://ci.example.com";
/// The origin a browser on it reports.
pub const ORIGIN: &str = "https://ci.example.com";
/// The clock starts here (a 0 `created_at` would read as "never fresh").
pub const T0: i64 = 1_000_000_000;

pub struct Harness {
    pub app: axum::Router,
    /// The engine `Db` the app serves: a test drives Runs on it.
    pub db: Arc<InMemoryDb>,
    pub clock: Arc<FakeClock>,
    pub sessions: Arc<InMemorySessions>,
    pub rbac: Arc<InMemoryRbac>,
    pub tokens: Arc<InMemoryApiTokens>,
    pub passkeys: Arc<InMemoryPasskeys>,
    pub step_up: Arc<InMemoryStepUp>,
    pub events: Arc<InMemorySecurityEvents>,
    /// The stores the app ACTUALLY uses — the in-memory ones above, or
    /// Postgres under [`Backend::Pg`].
    pub rbac_store: Arc<dyn RbacStore>,
    pub passkey_store: Arc<dyn PasskeyStore>,
    pub challenge_store: Arc<dyn ChallengeStore>,
    pub requests: Arc<dyn StepUpStore>,
    pub log: Arc<dyn SecurityEventLog>,
}

/// Where the step-up stores (and the role bindings the Org log joins) live.
pub enum Backend {
    Memory,
    /// A migrated Postgres: passkeys, challenges, confirmations, requests,
    /// the event log and the RBAC bindings.
    Pg(Arc<PostgresDb>),
}

/// A signed-in browser (or a Bearer client): the session id and its CSRF.
#[derive(Clone, Debug)]
pub struct Sess {
    pub id: String,
    pub csrf: String,
}

pub fn owner(subject: &str) -> Principal {
    Principal {
        subject: subject.into(),
        display_name: None,
        roles: vec![Role::Owner],
    }
}

pub fn scoped(subject: &str) -> Principal {
    Principal {
        subject: subject.into(),
        display_name: None,
        roles: vec![],
    }
}

/// An enforcing harness. `extra` adjusts the state before step-up is wired.
pub fn harness_with(extra: impl FnOnce(AppState) -> AppState) -> Harness {
    harness_on(Backend::Memory, extra)
}

/// [`harness_with`] over `backend`.
pub fn harness_on(backend: Backend, extra: impl FnOnce(AppState) -> AppState) -> Harness {
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(T0));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential("amy-code", owner("amy"))
            .with_credential("bob-code", scoped("bob")),
    );
    let sessions = Arc::new(InMemorySessions::new());
    let rbac = Arc::new(InMemoryRbac::new());
    let tokens = Arc::new(InMemoryApiTokens::new());
    let passkeys = Arc::new(InMemoryPasskeys::new());
    let step_up = Arc::new(InMemoryStepUp::new());
    let events = Arc::new(InMemorySecurityEvents::new().with_rbac(rbac.clone()));
    let challenges = Arc::new(InMemoryChallenges::new());
    #[allow(clippy::type_complexity)]
    let (rbac_store, passkey_store, challenge_store, requests, log): (
        Arc<dyn RbacStore>,
        Arc<dyn PasskeyStore>,
        Arc<dyn ChallengeStore>,
        Arc<dyn StepUpStore>,
        Arc<dyn SecurityEventLog>,
    ) = match backend {
        Backend::Memory => (
            rbac.clone(),
            passkeys.clone(),
            challenges,
            step_up.clone(),
            events.clone(),
        ),
        Backend::Pg(pg) => (pg.clone(), pg.clone(), pg.clone(), pg.clone(), pg),
    };
    let rp = RelyingParty::from_public_url(PUBLIC_URL, "Scarab").unwrap();
    let st = AppState::new(db.clone(), clock.clone(), logs)
        .with_auth(auth, sessions.clone())
        .with_rbac(rbac_store.clone())
        .with_api_tokens(tokens.clone())
        .with_secrets(Arc::new(FakeSecrets::new()))
        .with_public_url(PUBLIC_URL);
    let st = extra(st).with_step_up(StepUpService::new(
        passkey_store.clone(),
        challenge_store.clone(),
        requests.clone(),
        log.clone(),
        Arc::new(InMemorySealer::new()),
        rp,
    ));
    Harness {
        app: router(st),
        db,
        clock,
        sessions,
        rbac,
        tokens,
        passkeys,
        step_up,
        events,
        rbac_store,
        passkey_store,
        challenge_store,
        requests,
        log,
    }
}

pub fn harness() -> Harness {
    harness_with(|st| st)
}

impl Harness {
    pub async fn now(&self) -> i64 {
        scarab_engine::Clock::now(self.clock.as_ref()).await.0
    }

    /// A session for `principal` whose (browser) login completed now.
    pub async fn browser_session(&self, principal: Principal) -> Sess {
        self.session(principal, LOGIN_KIND_BROWSER).await
    }

    pub async fn session(&self, principal: Principal, login_kind: &str) -> Sess {
        let now = self.now().await;
        let s = Session {
            id: uuid::Uuid::new_v4().to_string(),
            principal,
            expires_at: now + 24 * 60 * 60 * 1000,
            csrf: uuid::Uuid::new_v4().to_string(),
            created_at: now,
            login_kind: login_kind.to_string(),
        };
        self.sessions.put(&s).await.unwrap();
        Sess {
            id: s.id,
            csrf: s.csrf,
        }
    }

    pub async fn grant(&self, subject: &str, scope: Scope, role: Role) {
        self.rbac_store
            .grant(
                &Binding {
                    subject: subject.into(),
                    scope,
                    role,
                },
                BindingOrigin::Native,
            )
            .await
            .unwrap();
    }

    pub async fn send(&self, req: Request<Body>) -> (StatusCode, Value) {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    /// A cookie request with the CSRF double-submit.
    pub async fn cookie(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        s: &Sess,
    ) -> (StatusCode, Value) {
        self.send(cookie_req(method, uri, body, s)).await
    }

    pub async fn bearer(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        token: &str,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        self.send(
            b.body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
                .unwrap(),
        )
        .await
    }

    /// Enroll `soft` as a passkey of the session's principal (the session's
    /// login must be fresh, or a window/action already confirmed for a
    /// later passkey). Returns the stored passkey's DTO.
    pub async fn enroll(&self, s: &Sess, soft: &mut SoftPasskey, label: &str) -> Value {
        let (st, opts) = self
            .cookie(
                "POST",
                "/v1/me/passkeys/registration/options",
                Some(json!({ "label": label })),
                s,
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{opts}");
        self.finish_enroll(s, soft, &opts).await
    }

    pub async fn finish_enroll(&self, s: &Sess, soft: &mut SoftPasskey, opts: &Value) -> Value {
        let cred = soft.register(&opts["public_key"], ORIGIN).unwrap();
        let (st, dto) = self
            .cookie(
                "POST",
                "/v1/me/passkeys/registration",
                Some(json!({ "challenge_id": opts["id"], "credential": cred })),
                s,
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{dto}");
        dto
    }

    /// Answer `challenge` (`{id, public_key}`, from a 403 or from
    /// `/v1/me/step-up/options`) with `soft`.
    pub async fn answer(
        &self,
        s: &Sess,
        soft: &mut SoftPasskey,
        challenge: &Value,
    ) -> (StatusCode, Value) {
        let cred = soft.assert(&challenge["public_key"], ORIGIN).unwrap();
        self.cookie(
            "POST",
            "/v1/me/step-up",
            Some(json!({ "challenge_id": challenge["id"], "credential": cred })),
            s,
        )
        .await
    }

    /// Open a 5-minute window in `s`.
    pub async fn open_window(&self, s: &Sess, soft: &mut SoftPasskey) -> Value {
        let (st, ch) = self.cookie("POST", "/v1/me/step-up/options", None, s).await;
        assert_eq!(st, StatusCode::OK, "{ch}");
        let (st, conf) = self.answer(s, soft, &ch).await;
        assert_eq!(st, StatusCode::OK, "{conf}");
        conf
    }

    /// Every event of `kind` in the log the app writes (whatever the
    /// backend), oldest first.
    pub async fn events(&self, kind: &str) -> Vec<SecurityEvent> {
        let mut all = self
            .log
            .list(&SecurityEventFilter {
                kinds: vec![kind.to_string()],
                limit: 10_000,
                ..Default::default()
            })
            .await
            .unwrap();
        all.reverse();
        all
    }

    /// The in-memory log only ([`Backend::Memory`]).
    pub fn events_of(&self, kind: &str) -> Vec<scarab_identity::SecurityEvent> {
        self.events
            .all()
            .into_iter()
            .filter(|e| e.kind == kind)
            .collect()
    }
}

pub fn cookie_req(method: &str, uri: &str, body: Option<Value>, s: &Sess) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("scarab_session={}", s.id))
        .header("x-csrf-token", &s.csrf);
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    b.body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
        .unwrap()
}

pub fn passkey() -> SoftPasskey {
    SoftPasskey::new(SoftAlg::Es256)
}

/// Is this a `403 step_up_required`?
pub fn is_step_up(status: StatusCode, body: &Value) -> bool {
    status == StatusCode::FORBIDDEN && body["code"] == "step_up_required"
}

#[track_caller]
pub fn assert_step_up(status: StatusCode, body: &Value, mode: &str) {
    assert!(
        is_step_up(status, body),
        "want 403 step_up_required, got {status}: {body}"
    );
    assert_eq!(body["mode"], mode, "{body}");
}
