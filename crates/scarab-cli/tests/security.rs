//! ADR-0073 §10 (gate criterion 8): `scarab security setup|status`, and the
//! CLI's report of a `403 step_up_required`, driven as the real `scarab`
//! binary against the REAL server router — in process, on an ephemeral port,
//! over the in-memory stores with step-up enforced. The passkey is a software
//! authenticator driving the real enrollment ceremony (the authenticator is
//! the one true external).

use std::io::{BufRead, BufReader};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_identity::{
    Binding, BindingOrigin, Principal, RbacStore, Role, Scope, Session, SessionStore,
    LOGIN_KIND_API, LOGIN_KIND_BROWSER,
};
use scarab_server::step_up::StepUpService;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeSecrets, InMemoryChallenges, InMemoryDb, InMemoryObjectStore,
    InMemoryPasskeys, InMemoryRbac, InMemorySecurityEvents, InMemorySessions,
    InMemoryStepUp,
};
use scarab_webauthn::soft::{SoftAlg, SoftPasskey};
use scarab_webauthn::RelyingParty;

/// The public URL the relying party (and every confirmation link) is
/// derived from. The CLI talks to the ephemeral port instead.
const PUBLIC_URL: &str = "https://ci.example.com";
const ORIGIN: &str = PUBLIC_URL;
/// The server's clock (a 0 `created_at` would read as "never fresh").
const T0: i64 = 1_000_000_000;
const DAY: i64 = 24 * 60 * 60 * 1000;

struct Server {
    /// `http://127.0.0.1:<port>` — what the CLI is pointed at.
    base: String,
    sessions: Arc<InMemorySessions>,
    rbac: Arc<InMemoryRbac>,
    http: reqwest::Client,
}

/// A signed-in principal: the session id (a CLI passes it as `--token`) and
/// its CSRF (a browser double-submits it).
struct Sess {
    id: String,
    csrf: String,
}

/// A unique suffix for session ids.
fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn scoped(subject: &str) -> Principal {
    Principal {
        subject: subject.into(),
        display_name: None,
        roles: vec![],
    }
}

/// The real router with login, RBAC, secrets and the registry (Project
/// `acme/web` bound, so the Org `acme` exists), and — when `enforce` — the
/// step-up service, served on an ephemeral port.
async fn serve(enforce: bool) -> Server {
    serve_with(enforce, |st| st).await
}

/// [`serve`], with `extra` adjusting the state (login providers, say).
async fn serve_with(enforce: bool, extra: impl FnOnce(AppState) -> AppState) -> Server {
    let db = Arc::new(InMemoryDb::new());
    db.put_connection(&ForgeConnection {
        id: "gh".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: "gh-token".into(),
    })
    .await
    .unwrap();
    db.bind_repo(
        "gh",
        &RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        "acme",
        "web",
    )
    .await
    .unwrap();
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let sessions = Arc::new(InMemorySessions::new());
    let rbac = Arc::new(InMemoryRbac::new());
    let mut st = AppState::new(db.clone(), Arc::new(FakeClock::new(T0)), logs)
        .with_auth(Arc::new(FakeAuthenticator::new()), sessions.clone())
        .with_rbac(rbac.clone())
        .with_secrets(Arc::new(FakeSecrets::new()))
        .with_forge_connections(db.clone())
        .with_public_url(PUBLIC_URL);
    st = extra(st);
    if enforce {
        st = st.with_step_up(StepUpService::new(
            Arc::new(InMemoryPasskeys::new()),
            Arc::new(InMemoryChallenges::new()),
            Arc::new(InMemoryStepUp::new()),
            Arc::new(InMemorySecurityEvents::new().with_rbac(rbac.clone())),
            // The production sealer; a lazy pool never connects.
            Arc::new(scarab_secrets_postgres::PostgresSecrets::with_keys(
                sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://unused@localhost/unused")
                    .unwrap(),
                scarab_secrets_postgres::MasterKeySet::new(vec![[7u8; 32]]).unwrap(),
            )),
            RelyingParty::from_public_url(PUBLIC_URL, "Scarab").unwrap(),
        ));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(st);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        base: format!("http://{addr}"),
        sessions,
        rbac,
        http: reqwest::Client::new(),
    }
}

impl Server {
    async fn session(&self, principal: Principal, login_kind: &str) -> Sess {
        let n = next_id();
        let s = Session {
            id: format!("sess-{}-{login_kind}-{n}", principal.subject),
            principal,
            expires_at: T0 + DAY,
            csrf: format!("csrf-{n}"),
            created_at: T0,
            login_kind: login_kind.to_string(),
        };
        self.sessions.put(&s).await.unwrap();
        Sess {
            id: s.id,
            csrf: s.csrf,
        }
    }

    /// A CLI session of `subject` (an API login, presented as Bearer).
    async fn cli(&self, subject: &str) -> Sess {
        self.session(scoped(subject), LOGIN_KIND_API).await
    }

    async fn grant(&self, subject: &str, role: Role) {
        self.rbac
            .grant(
                &Binding {
                    subject: subject.into(),
                    scope: Scope::Org("acme".into()),
                    role,
                },
                BindingOrigin::Native,
            )
            .await
            .unwrap();
    }

    /// A browser request: session cookie + CSRF double-submit.
    async fn cookie(&self, path: &str, body: Value, s: &Sess) -> (u16, Value) {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .header("cookie", format!("scarab_session={}", s.id))
            .header("x-csrf-token", &s.csrf)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    /// Enroll a software passkey for `subject` through the real ceremony,
    /// from a browser login that completed just now.
    async fn enroll(&self, subject: &str, label: &str) {
        let s = self.session(scoped(subject), LOGIN_KIND_BROWSER).await;
        let (st, opts) = self
            .cookie(
                "/v1/me/passkeys/registration/options",
                json!({ "label": label }),
                &s,
            )
            .await;
        assert_eq!(st, 200, "{opts}");
        let cred = SoftPasskey::new(SoftAlg::Es256)
            .register(&opts["public_key"], ORIGIN)
            .unwrap();
        let (st, dto) = self
            .cookie(
                "/v1/me/passkeys/registration",
                json!({ "challenge_id": opts["id"], "credential": cred }),
                &s,
            )
            .await;
        assert_eq!(st, 201, "{dto}");
    }
}

/// The real `scarab` binary, pointed at `server` as `token`.
fn scarab(server: &Server, token: &Sess, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_scarab"));
    c.args(args)
        .args(["--server", &server.base, "--token", &token.id])
        .env_remove("SCARAB_SERVER")
        .env_remove("SCARAB_TOKEN")
        .stdin(Stdio::null());
    c
}

/// Run to completion off the async runtime (the server lives on it).
async fn output(mut cmd: Command) -> Output {
    tokio::task::spawn_blocking(move || cmd.output().expect("run scarab"))
        .await
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_waits_for_a_passkey_enrolled_mid_poll_and_is_idempotent() {
    let srv = serve(true).await;
    let amy = srv.cli("amy").await;

    let mut child = scarab(
        &srv,
        &amy,
        &["security", "setup", "--no-browser", "--timeout", "60"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn scarab");
    let (tx, rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let next = |rx: mpsc::Receiver<String>| async move {
        tokio::task::spawn_blocking(move || {
            let line = rx.recv_timeout(Duration::from_secs(30));
            (line, rx)
        })
        .await
        .unwrap()
    };

    // The CLI saw no passkey: it prints the fresh-login URL and starts
    // polling. Nothing is enrolled until we have read that far.
    let mut rx = rx;
    let mut seen = Vec::new();
    let url = loop {
        let (line, back) = next(rx).await;
        rx = back;
        let line = line.unwrap_or_else(|e| panic!("no login URL ({e}); saw {seen:?}"));
        seen.push(line.clone());
        if line.contains("/v1/auth/login") {
            break line;
        }
    };
    assert_eq!(
        url.trim(),
        format!(
            "{}/v1/auth/login?next=%2Faccount%2Fsecurity%3Fenroll%3D1",
            srv.base
        ),
        "a fresh login that lands on enrollment"
    );
    assert!(
        seen.iter().any(|l| l.contains("`amy`")),
        "names the subject the CLI acts for: {seen:?}"
    );

    // The human enrolls in the browser while the CLI polls.
    srv.enroll("amy", "Laptop").await;

    let status = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(st) = child.try_wait().unwrap() {
                return Some((st, child));
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })
    .await
    .unwrap();
    let (status, mut child) = status.expect("setup returns once the passkey exists");
    // Every line up to EOF: the reader thread ends (and drops its sender)
    // once the exited child's stdout closes.
    let rest: Vec<String> = tokio::task::spawn_blocking(move || rx.iter().collect())
        .await
        .unwrap();
    let mut err = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut err).unwrap();
    assert!(
        status.success(),
        "exit {status:?}; stdout {rest:?}; stderr {err}"
    );
    assert!(
        rest.iter()
            .any(|l| l.contains("passkey enrolled: 1 passkey(s)")),
        "{rest:?}"
    );

    // A second run is a no-op: already enrolled, no login URL.
    let out = output(scarab(&srv, &amy, &["security", "setup", "--no-browser"])).await;
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", text(&out.stderr));
    assert!(
        stdout.contains("already enrolled: 1 passkey(s)"),
        "{stdout}"
    );
    assert!(!stdout.contains("/v1/auth/login"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_without_a_passkey_times_out_with_exit_1() {
    let srv = serve(true).await;
    let amy = srv.cli("amy").await;
    let out = output(scarab(
        &srv,
        &amy,
        &["security", "setup", "--no-browser", "--timeout", "1"],
    ))
    .await;
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(stdout.contains("/v1/auth/login?next="), "{stdout}");
    assert!(stderr.contains("timed out"), "{stderr}");
}

/// An OAuth login provider the server can redirect to (never called).
fn provider(id: &str, unqualified: bool) -> Arc<scarab_server::oauth::OAuthAuthenticator> {
    Arc::new(scarab_server::oauth::OAuthAuthenticator::new(
        scarab_server::config::OAuthConfig {
            client_id: "cid".into(),
            client_secret: "sekret".into(),
            authorize_url: format!("https://{id}.example.com/authorize"),
            token_url: format!("https://{id}.example.com/token"),
            userinfo_url: format!("https://{id}.example.com/user"),
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

/// git-bug 584bd03: with several login providers and no default an unnamed
/// login is a 400, so `security setup` names the provider this CLI's human
/// signs in with — chosen through `GET /v1/auth/providers` by the subject's
/// prefix — and the URL it prints actually reaches that provider.
#[tokio::test(flavor = "multi_thread")]
async fn setup_names_the_login_provider_when_there_is_no_default() {
    let srv = serve_with(true, |st| {
        st.with_oauth_login(provider("gitea", false))
            .with_oauth_login(provider("okta", false))
    })
    .await;
    let amy = srv.cli("okta:amy").await;
    let out = output(scarab(
        &srv,
        &amy,
        &["security", "setup", "--no-browser", "--timeout", "1"],
    ))
    .await;
    let stdout = text(&out.stdout);
    let url = stdout
        .lines()
        .find(|l| l.contains("/v1/auth/login"))
        .unwrap_or_else(|| panic!("no login URL: {stdout}"))
        .trim()
        .to_string();
    assert!(url.contains("provider=okta"), "{url}");
    assert!(
        url.contains("next=%2Faccount%2Fsecurity%3Fenroll%3D1"),
        "{url}"
    );
    // Following it reaches Okta, not a 400.
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(&url)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302, "{url}");
    let to = resp.headers()["location"].to_str().unwrap();
    assert!(to.starts_with("https://okta.example.com/authorize"), "{to}");
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_against_a_server_with_step_up_off_says_so() {
    let srv = serve(false).await;
    let amy = srv.cli("amy").await;
    let out = output(scarab(&srv, &amy, &["security", "setup", "--no-browser"])).await;
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", text(&out.stderr));
    assert!(stdout.contains("step-up is off"), "{stdout}");
    assert!(!stdout.contains("/v1/auth/login"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn status_lists_passkeys_and_shows_an_owner_the_admins_without_one() {
    let srv = serve(true).await;
    srv.grant("amy", Role::Owner).await;
    srv.grant("bob", Role::Admin).await;
    srv.enroll("amy", "Laptop").await;
    let amy = srv.cli("amy").await;
    let bob = srv.cli("bob").await;

    // A refusal in the Org: bob's CLI writes a secret.
    let resp = srv
        .http
        .post(format!("{}/v1/secrets", srv.base))
        .bearer_auth(&bob.id)
        .json(&json!({"org": "acme", "name": "DB_PASSWORD", "value": "hunter2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    let out = output(scarab(&srv, &amy, &["security", "status"])).await;
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", text(&out.stderr));
    assert!(stdout.contains("step-up: enforced"), "{stdout}");
    assert!(stdout.contains("passkeys (1):"), "{stdout}");
    assert!(stdout.contains("Laptop"), "{stdout}");
    assert!(stdout.contains("last used"), "{stdout}");
    assert!(
        stdout.contains("org acme (you are owner): 2 admin(s), 1 without a passkey"),
        "{stdout}"
    );
    assert!(
        stdout.contains("    - bob\n"),
        "bob has no passkey: {stdout}"
    );
    assert!(!stdout.contains("    - amy\n"), "amy has one: {stdout}");
    assert!(
        stdout
            .lines()
            .any(|l| l.contains("bob") && l.contains("secret.put")),
        "bob's refusal is listed: {stdout}"
    );
    assert!(!stdout.contains("hunter2"), "{stdout}");

    // bob is an admin, not the owner: his own state, no Org posture.
    let out = output(scarab(&srv, &bob, &["security", "status"])).await;
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", text(&out.stderr));
    assert!(stdout.contains("passkeys: none"), "{stdout}");
    assert!(!stdout.contains("org acme"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crown_jewel_call_prints_the_confirmation_link() {
    let srv = serve(true).await;
    srv.grant("amy", Role::Owner).await;
    srv.enroll("amy", "Laptop").await;
    let amy = srv.cli("amy").await;

    let out = output(scarab(
        &srv,
        &amy,
        &[
            "api",
            "/v1/secrets",
            "--data",
            r#"{"org":"acme","name":"DB_PASSWORD","value":"hunter2"}"#,
        ],
    ))
    .await;
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "This needs your confirmation in a browser: {PUBLIC_URL}/security/confirm/"
        )),
        "{stderr}"
    );
    assert!(stderr.contains("(expires 1970-01-12 "), "{stderr}");
    assert!(stderr.contains("Write a secret"), "{stderr}");
    assert!(!stderr.contains("hunter2"), "{stderr}");
}
