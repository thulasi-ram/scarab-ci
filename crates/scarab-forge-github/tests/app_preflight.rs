//! The GitHub App preflight/doctor — [`ForgePort::describe_capabilities`],
//! ADR-0060, git-bug `90644c6` — driven over REAL HTTP against a stub GitHub.
//!
//! Why this file exists at all: the preflight can never be exercised offline
//! against the real thing, and `tests/contract_live.rs` needs credentials
//! nobody has in CI. So these tests are the *only* standing evidence that the
//! feature works. `capabilities_from_app` is unit-tested in `src/lib.rs`, but
//! that only proves the mapping — it says nothing about which endpoint is
//! called, what credential is presented, or what happens when GitHub says no.
//! Everything below the mapping is what breaks in production, so it is what is
//! pinned here.
//!
//! The forge is a true external (ADR-0017), so the double is a real server on a
//! real socket, not a mock object: `reqwest`, the router, the headers and the
//! status handling are all the production ones. The stub *records* requests so
//! the tests can assert on what went out — that is a captured wire observation,
//! not a mock call-log assertion; the claim being made ("the preflight
//! authenticates as the App at `GET /app`") is a claim about GitHub's API
//! contract and has no other observable.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use scarab_forge::{ForgeError, ForgePort};
use scarab_forge_github::{GithubApp, GithubForge};

// ---------------------------------------------------------------- stub GitHub

/// One scripted reply, popped in order.
struct Reply {
    status: StatusCode,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Reply {
    fn new(status: StatusCode, body: &str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    fn ok(body: &str) -> Self {
        Self::new(StatusCode::OK, body)
    }

    fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

/// What the stub saw — the wire observation the assertions are made against.
#[derive(Clone)]
struct Seen {
    method: String,
    path: String,
    authorization: Option<String>,
    accept: Option<String>,
}

#[derive(Default)]
struct Recorder {
    seen: Vec<Seen>,
    replies: VecDeque<Reply>,
}

struct Stub {
    base_url: String,
    recorder: Arc<Mutex<Recorder>>,
}

impl Stub {
    async fn start(replies: Vec<Reply>) -> Self {
        let recorder = Arc::new(Mutex::new(Recorder {
            seen: Vec::new(),
            replies: replies.into(),
        }));
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(recorder.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base_url: format!("http://{addr}"),
            recorder,
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.recorder.lock().unwrap().seen.clone()
    }

    /// A forge in App mode pointed at this stub.
    fn app_forge(&self) -> GithubForge {
        GithubForge::app(app_key().app.clone()).with_base_url(&self.base_url)
    }
}

async fn handle(
    State(recorder): State<Arc<Mutex<Recorder>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    _body: Bytes,
) -> Response {
    let str_header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let reply = {
        let mut r = recorder.lock().unwrap();
        r.seen.push(Seen {
            method: method.to_string(),
            path: uri.path().to_string(),
            authorization: str_header("authorization"),
            accept: str_header("accept"),
        });
        r.replies.pop_front()
    };
    let Some(reply) = reply else {
        // Running out of scripted replies is itself the finding: the adapter
        // issued a request the test did not expect (a retry that should not
        // have happened). The recorded `seen` count is what fails the test.
        return (StatusCode::IM_A_TEAPOT, "unscripted request").into_response();
    };
    let mut resp = (reply.status, reply.body).into_response();
    for (name, value) in reply.headers {
        resp.headers_mut().insert(name, value.parse().unwrap());
    }
    resp
}

// ------------------------------------------------------------ App credentials

struct AppKey {
    app: GithubApp,
    /// The public half, so a test can verify the JWT the adapter signed.
    public_pem: String,
}

/// One throwaway RSA keypair for the whole binary — 2048-bit keygen is slow.
fn app_key() -> &'static AppKey {
    static KEY: OnceLock<AppKey> = OnceLock::new();
    KEY.get_or_init(|| {
        use rsa::pkcs1::EncodeRsaPublicKey;
        use rsa::pkcs8::EncodePrivateKey;
        let mut rng = rsa::rand_core::OsRng;
        let private = rsa::RsaPrivateKey::new(&mut rng, 2048).expect("generate key");
        AppKey {
            app: GithubApp {
                app_id: "4323648".into(),
                private_key_pem: private
                    .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
                    .expect("encode private")
                    .to_string(),
            },
            public_pem: rsa::RsaPublicKey::from(&private)
                .to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)
                .expect("encode public"),
        }
    })
}

/// Verify `jwt` against the App's public key and return its `iss` claim.
///
/// This is the assertion that cannot be made any other way: it proves the
/// credential on the wire is an RS256 JWT **this App's private key signed**,
/// not a PAT, not an installation token, not the PEM itself.
fn verified_jwt_issuer(jwt: &str) -> String {
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.set_required_spec_claims(&["exp", "iss"]);
    let decoded = jsonwebtoken::decode::<serde_json::Value>(
        jwt,
        &jsonwebtoken::DecodingKey::from_rsa_pem(app_key().public_pem.as_bytes()).unwrap(),
        &validation,
    )
    .expect("the App's public key verifies the JWT the adapter sent");
    decoded.claims["iss"]
        .as_str()
        .expect("iss claim")
        .to_string()
}

fn api_error(err: ForgeError) -> String {
    match err {
        ForgeError::Api(msg) => msg,
        other => panic!("expected ForgeError::Api, got {other:?}"),
    }
}

// ------------------------------------------------------------------- the tests

/// The acceptance case: a well-formed `GET /app` body becomes the operator's
/// capability readout, and the call that fetched it is the one GitHub
/// documents — `GET /app`, App-JWT authenticated, on the pinned API version.
#[tokio::test]
async fn preflight_reads_the_app_record_and_authenticates_as_the_app() {
    // Trimmed from a real `GET /app` response for the scarab-ci App.
    let body = serde_json::json!({
        "id": 4323648,
        "slug": "scarab-ci",
        "permissions": {
            "checks": "write",
            "contents": "read",
            "metadata": "read",
            "statuses": "write"
        },
        "events": ["push", "pull_request"]
    })
    .to_string();
    let stub = Stub::start(vec![Reply::ok(&body)]).await;

    let caps = stub
        .app_forge()
        .describe_capabilities()
        .await
        .expect("preflight succeeds");

    // What the doctor shows the operator.
    assert_eq!(
        caps.permissions.get("statuses").map(String::as_str),
        Some("write"),
        "statuses:write is what lets Scarab report a Run back to GitHub"
    );
    assert_eq!(
        caps.permissions.get("contents").map(String::as_str),
        Some("read")
    );
    assert_eq!(
        caps.permissions.len(),
        4,
        "every granted permission is kept"
    );
    assert!(caps.events.contains("push"));
    assert!(caps.events.contains("pull_request"));
    assert_eq!(caps.events.len(), 2);

    // ...and how it was asked for.
    let seen = stub.seen();
    assert_eq!(seen.len(), 1, "one request, no retry on success");
    assert_eq!(seen[0].method, "GET");
    assert_eq!(
        seen[0].path, "/app",
        "the App record, not an installation — the event subscription and the \
         permission ceiling are App-level settings"
    );
    let jwt = seen[0]
        .authorization
        .as_deref()
        .and_then(|h| h.strip_prefix("Bearer "))
        .expect("Bearer authorization header");
    assert_eq!(
        verified_jwt_issuer(jwt),
        app_key().app.app_id,
        "GET /app is authenticated with the App's own JWT; an installation \
         token gets a 403 here"
    );
    assert_eq!(
        seen[0].accept.as_deref(),
        Some("application/vnd.github+json"),
        "GitHub content-negotiates; the wrong Accept gets a different schema"
    );
}

/// A brand-new App — nothing granted, nothing subscribed — must still produce a
/// readout. Reporting "you have nothing" is the doctor's whole job; failing
/// instead would hide exactly the misconfiguration it exists to find.
#[tokio::test]
async fn an_app_granted_nothing_reports_nothing_rather_than_failing() {
    let stub = Stub::start(vec![Reply::ok("{}")]).await;
    let caps = stub
        .app_forge()
        .describe_capabilities()
        .await
        .expect("an empty App record is a valid readout, not an error");
    assert!(caps.permissions.is_empty());
    assert!(caps.events.is_empty());

    // Same requirement when the keys are present but the wrong JSON type —
    // GitHub sends `"permissions": null` for some App shapes, and a future
    // field could arrive as a scalar.
    let odd = serde_json::json!({ "permissions": null, "events": "push" }).to_string();
    let stub = Stub::start(vec![Reply::ok(&odd)]).await;
    let caps = stub
        .app_forge()
        .describe_capabilities()
        .await
        .expect("unexpected JSON types degrade to empty, not to an error");
    assert!(caps.permissions.is_empty());
    assert!(caps.events.is_empty(), "a string is not the events array");
}

/// Every non-2xx must surface as `Api` carrying the status **and GitHub's own
/// message** — the operator's only clue — and must not be retried. A 403 in
/// particular is a permission answer, not a rate-limit answer: retrying it
/// three times would triple the latency of a preflight that is already doomed.
#[tokio::test]
async fn http_failures_report_githubs_reason_and_are_not_retried() {
    let cases = [
        (
            StatusCode::UNAUTHORIZED,
            r#"{"message":"A JSON web token could not be decoded"}"#,
            "A JSON web token could not be decoded",
        ),
        (
            StatusCode::FORBIDDEN,
            r#"{"message":"Resource not accessible by integration"}"#,
            "Resource not accessible by integration",
        ),
        (
            StatusCode::NOT_FOUND,
            r#"{"message":"Not Found"}"#,
            "Not Found",
        ),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "upstream is on fire",
            "upstream is on fire",
        ),
    ];
    for (status, body, needle) in cases {
        let stub = Stub::start(vec![Reply::new(status, body)]).await;
        let err = stub
            .app_forge()
            .describe_capabilities()
            .await
            .expect_err("a non-2xx is a failure, never an empty capability set");
        let msg = api_error(err);
        assert!(
            msg.contains(status.as_str()),
            "{status} should name itself in the error, got: {msg}"
        );
        assert!(
            msg.contains(needle),
            "{status} should carry GitHub's message, got: {msg}"
        );
        assert_eq!(
            stub.seen().len(),
            1,
            "{status} is a verdict, not backpressure — it must not be retried"
        );
    }
}

/// A 200 whose body is not JSON — a GHES proxy's HTML login page, the classic
/// wrong-base-url symptom. It must be an error naming the parse failure, and
/// emphatically not an empty readout: "your App has no permissions" would send
/// the operator to fix the wrong thing.
#[tokio::test]
async fn a_non_json_body_is_a_parse_error_not_an_empty_readout() {
    let stub = Stub::start(vec![Reply::ok("<html><body>Sign in</body></html>")]).await;
    let err = stub
        .app_forge()
        .describe_capabilities()
        .await
        .expect_err("HTML is not an App record");
    let msg = api_error(err);
    assert!(
        msg.contains("bad JSON"),
        "should name the parse failure: {msg}"
    );
}

/// Fixed-token (PAT) mode has no App to introspect. It must answer
/// `Unsupported` — which callers degrade on by hiding the doctor — **without
/// spending a request**: `/app` rejects a PAT with a 403 that reads exactly
/// like "your App is misconfigured".
#[tokio::test]
async fn a_fixed_token_connection_is_unsupported_and_never_calls_github() {
    let stub = Stub::start(vec![]).await;
    let forge = GithubForge::new("ghp_not_an_app").with_base_url(&stub.base_url);

    let err = forge
        .describe_capabilities()
        .await
        .expect_err("a PAT cannot introspect an App");
    assert!(
        matches!(err, ForgeError::Unsupported(_)),
        "must be Unsupported (degrade the UI), not Api (report a forge outage): {err:?}"
    );
    assert!(
        stub.seen().is_empty(),
        "no request should reach GitHub: its 403 would be misread as a broken App"
    );
}

/// The likeliest thing an operator gets wrong: a private key that is truncated,
/// double-pasted, or in the wrong encoding. The preflight exists to name that,
/// so the error must point at the key — and must not have burned a request on
/// GitHub first, which would return a 401 and blame the JWT instead.
#[tokio::test]
async fn an_unusable_private_key_is_named_before_anything_is_sent() {
    let stub = Stub::start(vec![]).await;
    let forge = GithubForge::app(GithubApp {
        app_id: "4323648".into(),
        // A PEM header with nothing behind it — what a half-copied secret looks like.
        private_key_pem: "-----BEGIN RSA PRIVATE KEY-----\n-----END RSA PRIVATE KEY-----\n".into(),
    })
    .with_base_url(&stub.base_url);

    let err = forge
        .describe_capabilities()
        .await
        .expect_err("an unusable key cannot sign the App JWT");
    let msg = api_error(err);
    assert!(
        msg.contains("App private key"),
        "the operator must be told which secret is wrong, got: {msg}"
    );
    assert!(
        stub.seen().is_empty(),
        "signing fails locally; nothing should reach GitHub"
    );
}

/// GitHub's *secondary* rate limit is a 403 carrying `Retry-After`. That one
/// must be waited out and retried — and the retrying must be bounded, or a
/// forge that keeps saying "slow down" wedges the caller forever.
#[tokio::test]
async fn a_secondary_rate_limit_is_retried_and_the_retrying_is_bounded() {
    let ok = serde_json::json!({
        "permissions": { "contents": "read" },
        "events": ["push"]
    })
    .to_string();
    // `retry-after: 0` keeps the real sleep instant while still exercising the
    // header-parsing branch that decides how long to wait.
    let limited = || {
        Reply::new(
            StatusCode::FORBIDDEN,
            "You have exceeded a secondary rate limit",
        )
        .header("retry-after", "0")
    };

    let stub = Stub::start(vec![limited(), Reply::ok(&ok)]).await;
    let caps = stub
        .app_forge()
        .describe_capabilities()
        .await
        .expect("the retry should have succeeded");
    assert_eq!(
        caps.permissions.get("contents").map(String::as_str),
        Some("read"),
        "the retried response is the one that gets parsed"
    );
    assert_eq!(stub.seen().len(), 2, "exactly one retry was needed");

    // A forge stuck on "slow down": give up and report, bounded by
    // MAX_HTTP_ATTEMPTS. Four replies are scripted so that a fourth attempt
    // would be served rather than 418'd — the count assertion is the test.
    let stub = Stub::start(vec![limited(), limited(), limited(), Reply::ok(&ok)]).await;
    let err = stub
        .app_forge()
        .describe_capabilities()
        .await
        .expect_err("persistent rate limiting must eventually surface");
    assert!(api_error(err).contains("secondary rate limit"));
    assert_eq!(
        stub.seen().len(),
        3,
        "bounded at MAX_HTTP_ATTEMPTS; an unbounded loop would hammer GitHub"
    );
}

/// The *primary* rate limit has a different shape: 403 with the remaining
/// budget at zero and **no** `Retry-After`. It is still backpressure and must
/// still be retried — treating it as a hard failure turns a transient into a
/// broken preflight. Costs a real ~2s backoff (the fallback `2 * attempt`),
/// which is why only one attempt is provoked.
#[tokio::test]
async fn an_exhausted_rate_limit_window_is_also_backpressure() {
    let ok = serde_json::json!({ "permissions": { "contents": "read" } }).to_string();
    let stub = Stub::start(vec![
        Reply::new(StatusCode::FORBIDDEN, "API rate limit exceeded")
            .header("x-ratelimit-remaining", "0"),
        Reply::ok(&ok),
    ])
    .await;

    let caps = stub
        .app_forge()
        .describe_capabilities()
        .await
        .expect("an exhausted window is retried, not reported as a failure");
    assert_eq!(
        caps.permissions.get("contents").map(String::as_str),
        Some("read")
    );
    assert_eq!(stub.seen().len(), 2);
}
