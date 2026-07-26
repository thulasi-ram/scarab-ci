//! Browser + API OAuth/OIDC login flow (ADR-0049 C1 and its hardening
//! amendment) against a stub provider served over real HTTP — the provider is
//! the one true external here, so it is a real server with a real RSA key, not
//! a mock object.
//!
//! Covered: the redirect carries client_id/redirect_uri/state **and the PKCE
//! S256 challenge**, planting the flow cookie; the callback verifies the state
//! echo, replays the `code_verifier`, exchanges the code, and mints the session
//! + CSRF cookies; a forged state is rejected; a **verified `id_token`** is the
//! identity and beats userinfo; an invalid one (forged signature, wrong `aud`,
//! wrong `iss`, expired, wrong/missing `nonce`) fails the login and NEVER falls
//! back to userinfo; a provider that returns no `id_token` (the GitHub shape)
//! still logs in via userinfo; the owners allowlist maps roles by subject or by
//! VERIFIED email only.

use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use base64::Engine;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use scarab_server::config::OAuthConfig;
use scarab_server::oauth::OAuthAuthenticator;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore, InMemorySessions};

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The provider's signing material. Generated once per test binary (RSA keygen
/// is expensive): the published key, plus a `rogue` key the JWKS never
/// publishes — a token signed with it is a forged signature under a known `kid`.
struct SigningKeys {
    kid: String,
    private_pem: String,
    rogue_pem: String,
    n: String,
    e: String,
}

fn keys() -> &'static SigningKeys {
    static KEYS: OnceLock<SigningKeys> = OnceLock::new();
    KEYS.get_or_init(|| {
        let mut rng = rand::rngs::OsRng;
        let private = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let rogue = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let public = rsa::RsaPublicKey::from(&private);
        SigningKeys {
            kid: "idp-key-1".into(),
            private_pem: private.to_pkcs8_pem(LineEnding::LF).unwrap().to_string(),
            rogue_pem: rogue.to_pkcs8_pem(LineEnding::LF).unwrap().to_string(),
            n: b64url(&public.n().to_bytes_be()),
            e: b64url(&public.e().to_bytes_be()),
        }
    })
}

/// How the stub shapes the `id_token` it returns. The default is a *valid* one;
/// every rejection case is one field bent away from it.
#[derive(Clone)]
struct IdToken {
    /// An opaque per-client subject — what a real OIDC issuer hands out.
    sub: String,
    /// `None` = the stub's own base URL (the honest issuer).
    iss: Option<String>,
    aud: String,
    /// Seconds from now; negative = already expired.
    exp_in: i64,
    nonce: Option<String>,
    email: Option<String>,
    email_verified: Option<serde_json::Value>,
    /// Sign with the key the JWKS does not publish.
    rogue_signature: bool,
}

impl Default for IdToken {
    fn default() -> Self {
        Self {
            sub: "8f3c-opaque-uuid".into(),
            iss: None,
            aud: "cid".into(),
            exp_in: 600,
            nonce: None,
            email: None,
            email_verified: None,
            rogue_signature: false,
        }
    }
}

fn sign_id_token(spec: &IdToken, issuer: &str) -> String {
    let k = keys();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut claims = serde_json::json!({
        "iss": spec.iss.clone().unwrap_or_else(|| issuer.to_string()),
        "aud": spec.aud.clone(),
        "sub": spec.sub.clone(),
        "iat": now,
        "exp": now + spec.exp_in,
        "name": "Ada Lovelace",
    });
    if let Some(nonce) = &spec.nonce {
        claims["nonce"] = serde_json::json!(nonce);
    }
    if let Some(email) = &spec.email {
        claims["email"] = serde_json::json!(email);
    }
    if let Some(verified) = &spec.email_verified {
        claims["email_verified"] = verified.clone();
    }
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(k.kid.clone());
    let pem = if spec.rogue_signature {
        &k.rogue_pem
    } else {
        &k.private_pem
    };
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap();
    jsonwebtoken::encode(&header, &claims, &key).unwrap()
}

/// A running stub provider: exchanges the fixed code `good-code`, knows one
/// forge user (`alice`) at userinfo, publishes an OIDC discovery doc + JWKS, and
/// records what it was actually sent.
#[derive(Clone)]
struct Provider {
    base: String,
    /// `None` = plain OAuth2: no `id_token` at all (the GitHub shape).
    id_token: Arc<Mutex<Option<IdToken>>>,
    /// The `code_verifier` seen at `/token`, per exchange (`None` = absent).
    verifiers: Arc<Mutex<Vec<Option<String>>>>,
    /// How often userinfo was consulted — `0` is how "no silent fallback" is
    /// proven rather than asserted.
    userinfo_hits: Arc<Mutex<usize>>,
}

/// Read one field out of an `application/x-www-form-urlencoded` body.
fn form_field(body: &str, name: &str) -> Option<String> {
    body.split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
        .map(str::to_string)
}

async fn provider(id_token: Option<IdToken>) -> Provider {
    // Bind first: the issuer URL is part of the id_token the same server signs.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let p = Provider {
        base: base.clone(),
        id_token: Arc::new(Mutex::new(id_token)),
        verifiers: Arc::new(Mutex::new(Vec::new())),
        userinfo_hits: Arc::new(Mutex::new(0)),
    };

    let token_p = p.clone();
    let info_p = p.clone();
    let disco_base = base.clone();
    let app = axum::Router::new()
        .route(
            "/token",
            post(move |body: String| {
                let p = token_p.clone();
                async move {
                    assert!(body.contains("grant_type=authorization_code"), "{body}");
                    p.verifiers
                        .lock()
                        .unwrap()
                        .push(form_field(&body, "code_verifier"));
                    if !body.contains("code=good-code") {
                        return (StatusCode::BAD_REQUEST, "bad code").into_response();
                    }
                    let mut token =
                        serde_json::json!({ "access_token": "at-1", "token_type": "bearer" });
                    let spec = p.id_token.lock().unwrap().clone();
                    if let Some(spec) = spec {
                        token["id_token"] =
                            serde_json::Value::String(sign_id_token(&spec, &p.base));
                    }
                    axum::Json(token).into_response()
                }
            }),
        )
        .route(
            "/user",
            get(move |headers: axum::http::HeaderMap| {
                let p = info_p.clone();
                async move {
                    assert_eq!(
                        headers.get("authorization").and_then(|v| v.to_str().ok()),
                        Some("Bearer at-1")
                    );
                    *p.userinfo_hits.lock().unwrap() += 1;
                    axum::Json(serde_json::json!({ "login": "alice", "name": "Alice A" }))
                }
            }),
        )
        .route(
            "/.well-known/openid-configuration",
            get(move || {
                let base = disco_base.clone();
                async move {
                    axum::Json(serde_json::json!({
                        "issuer": base.clone(),
                        "jwks_uri": format!("{base}/jwks.json"),
                        "id_token_signing_alg_values_supported": ["RS256"],
                    }))
                }
            }),
        )
        .route(
            "/jwks.json",
            get(|| async {
                let k = keys();
                axum::Json(serde_json::json!({ "keys": [{
                    "kty": "RSA", "use": "sig", "alg": "RS256",
                    "kid": k.kid.clone(), "n": k.n.clone(), "e": k.e.clone(),
                }] }))
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    p
}

/// An app wired to `p`. `oidc` = configure the issuer, i.e. OIDC mode where a
/// returned `id_token` is verified and authoritative.
fn app_for(p: &Provider, owners: Vec<String>, oidc: bool) -> axum::Router {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    let login = Arc::new(OAuthAuthenticator::new(OAuthConfig {
        client_id: "cid".into(),
        client_secret: "sekret".into(),
        authorize_url: format!("{}/authorize", p.base),
        token_url: format!("{}/token", p.base),
        userinfo_url: format!("{}/user", p.base),
        oidc_issuer: if oidc { Some(p.base.clone()) } else { None },
        scopes: "read:user".into(),
        owners,
    }));
    router(
        AppState::new(db, clock, logs)
            .with_public_url("https://scarab.example.com")
            .with_auth(login.clone(), Arc::new(InMemorySessions::new()))
            .with_oauth_login(login),
    )
}

fn cookie_from(resp: &axum::response::Response, name: &str) -> Option<String> {
    resp.headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| {
            c.strip_prefix(&format!("{name}="))
                .map(|rest| rest.split(';').next().unwrap_or("").to_string())
        })
}

/// The login secrets the server planted, read the way the browser would — and
/// split here (not via the production parser) so the test is an independent
/// check on the cookie's shape: `state.verifier.nonce`.
struct Flow {
    state: String,
    verifier: String,
    nonce: String,
}

fn parse_flow(cookie: &str) -> Flow {
    let parts: Vec<&str> = cookie.split('.').collect();
    assert_eq!(parts.len(), 3, "state cookie must carry state.verifier.nonce");
    for p in &parts {
        assert!(p.len() >= 20, "each component must be high-entropy: {p}");
    }
    Flow {
        state: parts[0].into(),
        verifier: parts[1].into(),
        nonce: parts[2].into(),
    }
}

/// `GET /v1/auth/login` → (authorize URL, planted flow).
async fn begin_login(app: &axum::Router) -> (String, Flow) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/auth/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    let location = resp.headers()["location"].to_str().unwrap().to_string();
    let cookie = cookie_from(&resp, "scarab_oauth_state").expect("state cookie");
    (location, parse_flow(&cookie))
}

/// `GET /v1/auth/callback` with the flow cookie replayed.
async fn callback(app: &axum::Router, flow: &Flow, code: &str) -> axum::response::Response {
    let cookie = format!(
        "scarab_oauth_state={}.{}.{}",
        flow.state, flow.verifier, flow.nonce
    );
    app.clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/auth/callback?code={code}&state={}", flow.state))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

/// The API/CLI path (`POST /v1/auth/login`) — the one that reports the resolved
/// subject in its body, which is how "whose claims won" is observed.
async fn api_login(app: &axum::Router, code: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "credential": code }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Whether the session may write: `Owner` creates the run, a `Viewer` is
/// forbidden. Bearer-authenticated, so no CSRF token is involved.
async fn may_write(app: &axum::Router, session: &str) -> bool {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {session}"))
                .body(Body::from(
                    serde_json::json!({
                        "pipeline": {
                            "ir_version": 1,
                            "steps": [{
                                "id": "build",
                                "image": "busybox:latest",
                                "command": ["true"]
                            }]
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    match resp.status() {
        StatusCode::CREATED => true,
        StatusCode::FORBIDDEN => false,
        other => panic!("unexpected write status {other}"),
    }
}

#[tokio::test]
async fn full_browser_login_flow_lands_a_session_with_csrf() {
    let p = provider(None).await;
    let app = app_for(&p, vec!["alice".into()], false);

    // 1. GET /v1/auth/login → 302 to the provider with state + redirect_uri +
    //    the PKCE challenge.
    let (location, flow) = begin_login(&app).await;
    assert!(
        location.starts_with(&format!("{}/authorize?", p.base)),
        "{location}"
    );
    assert!(location.contains("client_id=cid"), "{location}");
    assert!(
        location.contains("redirect_uri=https%3A%2F%2Fscarab.example.com%2Fv1%2Fauth%2Fcallback"),
        "{location}"
    );
    assert!(location.contains("scope=read%3Auser"), "{location}");
    assert!(
        location.contains(&format!("state={}", flow.state)),
        "{location}"
    );
    assert!(location.contains("code_challenge_method=S256"), "{location}");
    // Plain OAuth2 mode verifies no id_token, so it sends no nonce to check.
    assert!(!location.contains("nonce="), "{location}");

    // 2. The provider calls back with code + state; the state echo must match
    //    the cookie.
    let resp = callback(&app, &flow, "good-code").await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(resp.headers()["location"], "/");
    let session = cookie_from(&resp, "scarab_session").expect("session cookie");
    let csrf = cookie_from(&resp, "scarab_csrf").expect("csrf cookie");
    assert!(!session.is_empty() && !csrf.is_empty());

    // 3. The session is real: alice is an owner, so a cookie+CSRF mutation
    //    (secret put = Administer) is authorized end-to-end.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/orgs/acme/secrets/deploy_key")
                .header("content-type", "application/json")
                .header("cookie", format!("scarab_session={session}"))
                .header("x-csrf-token", &csrf)
                .body(Body::from(
                    serde_json::json!({ "value": "s3cret" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    // 404 would mean no secrets store is wired (it isn't in this app);
    // anything but 401/403 proves authn+authz accepted the browser session.
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_ne!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn callback_rejects_a_forged_state() {
    let p = provider(None).await;
    let app = app_for(&p, vec![], false);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/auth/callback?code=good-code&state=attacker-picked")
                .header("cookie", "scarab_oauth_state=the-real-one.verifier.nonce")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        p.verifiers.lock().unwrap().is_empty(),
        "a forged state must not reach the token endpoint at all"
    );
}

#[tokio::test]
async fn non_owner_logs_in_as_viewer() {
    let p = provider(None).await;
    // alice is NOT in the owners list this time.
    let app = app_for(&p, vec!["someone-else".into()], false);

    let (_, flow) = begin_login(&app).await;
    let resp = callback(&app, &flow, "good-code").await;
    let session = cookie_from(&resp, "scarab_session").unwrap();
    let csrf = cookie_from(&resp, "scarab_csrf").unwrap();

    // A Viewer may read…
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs")
                .header("cookie", format!("scarab_session={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // …but a mutation is forbidden even with a valid CSRF token.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .header("cookie", format!("scarab_session={session}"))
                .header("x-csrf-token", &csrf)
                .body(Body::from(
                    serde_json::json!({
                        "pipeline": {
                            "ir_version": 1,
                            "steps": [{ "id": "b", "image": "busybox", "command": ["true"] }]
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// PKCE (ADR-0049 amendment, RFC 7636)
// ---------------------------------------------------------------------------

/// The challenge on the redirect is S256 of the verifier in the cookie, and the
/// callback replays that exact verifier at the token endpoint — the whole point
/// of PKCE is that those two are the same secret.
#[tokio::test]
async fn pkce_challenge_is_sent_and_the_verifier_round_trips() {
    let p = provider(None).await;
    let app = app_for(&p, vec![], false);

    let (location, flow) = begin_login(&app).await;
    let expected = b64url(Sha256::digest(flow.verifier.as_bytes()).as_slice());
    assert!(
        location.contains(&format!("code_challenge={expected}")),
        "challenge must be S256(verifier): {location}"
    );
    assert!(location.contains("code_challenge_method=S256"), "{location}");
    assert!(
        !location.contains(&flow.verifier),
        "the verifier itself must never leave the server: {location}"
    );

    let resp = callback(&app, &flow, "good-code").await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        p.verifiers.lock().unwrap().as_slice(),
        &[Some(flow.verifier.clone())],
        "the token exchange must replay the cookie's verifier"
    );
}

/// The API/CLI path has no server-side browser flow, so it stays usable with no
/// verifier — and forwards one when the client ran its own authorize.
#[tokio::test]
async fn api_login_works_without_a_verifier_and_forwards_one_when_given() {
    let p = provider(None).await;
    let app = app_for(&p, vec!["alice".into()], false);

    let resp = api_login(&app, "good-code").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["subject"], "alice");
    assert_eq!(p.verifiers.lock().unwrap().as_slice(), &[None]);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "credential": "good-code",
                        "code_verifier": "cli-held-verifier",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        p.verifiers.lock().unwrap().as_slice(),
        &[None, Some("cli-held-verifier".into())]
    );
}

// ---------------------------------------------------------------------------
// id_token verification (ADR-0049 amendment)
// ---------------------------------------------------------------------------

/// With a real OIDC issuer the id_token IS the assertion: its `sub` becomes the
/// Principal and userinfo is not consulted at all.
#[tokio::test]
async fn verified_id_token_claims_beat_userinfo() {
    let p = provider(Some(IdToken::default())).await;
    let app = app_for(&p, vec!["8f3c-opaque-uuid".into()], true);

    let resp = api_login(&app, "good-code").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(
        v["subject"], "8f3c-opaque-uuid",
        "the verified id_token subject wins over userinfo's `alice`"
    );
    assert_eq!(
        *p.userinfo_hits.lock().unwrap(),
        0,
        "a verified id_token makes userinfo unnecessary"
    );
    let session = v["session"].as_str().unwrap();
    assert!(may_write(&app, session).await, "that subject is an owner");
}

/// Every way an id_token can be wrong: the login FAILS, and userinfo is never
/// consulted — a fallback here would make the verification decorative.
#[tokio::test]
async fn invalid_id_token_fails_login_and_never_falls_back_to_userinfo() {
    let cases: Vec<(&str, IdToken)> = vec![
        (
            "forged signature",
            IdToken {
                rogue_signature: true,
                ..Default::default()
            },
        ),
        (
            "wrong aud",
            IdToken {
                aud: "some-other-client".into(),
                ..Default::default()
            },
        ),
        (
            "wrong iss",
            IdToken {
                iss: Some("https://evil.example".into()),
                ..Default::default()
            },
        ),
        (
            "expired",
            IdToken {
                exp_in: -3_600,
                ..Default::default()
            },
        ),
    ];
    for (name, spec) in cases {
        let p = provider(Some(spec)).await;
        let app = app_for(&p, vec!["8f3c-opaque-uuid".into()], true);
        let resp = api_login(&app, "good-code").await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{name} must be rejected"
        );
        assert_eq!(
            *p.userinfo_hits.lock().unwrap(),
            0,
            "{name} must not fall back to userinfo"
        );
    }
}

/// The nonce binds the id_token to THIS authorize request. The browser flow
/// carries one, so a token with the wrong nonce (or none) is rejected.
#[tokio::test]
async fn id_token_nonce_must_match_the_login_flow() {
    let p = provider(Some(IdToken::default())).await;
    let app = app_for(&p, vec![], true);

    // Happy path: the issuer echoes the flow's nonce.
    let (location, flow) = begin_login(&app).await;
    assert!(
        location.contains(&format!("nonce={}", flow.nonce)),
        "OIDC mode must send the nonce: {location}"
    );
    *p.id_token.lock().unwrap() = Some(IdToken {
        nonce: Some(flow.nonce.clone()),
        ..Default::default()
    });
    let resp = callback(&app, &flow, "good-code").await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(cookie_from(&resp, "scarab_session").is_some());

    // A replayed/other-flow token carries a different nonce.
    let (_, flow) = begin_login(&app).await;
    *p.id_token.lock().unwrap() = Some(IdToken {
        nonce: Some("some-other-flow".into()),
        ..Default::default()
    });
    let resp = callback(&app, &flow, "good-code").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "wrong nonce");

    // No nonce at all, where one was demanded.
    let (_, flow) = begin_login(&app).await;
    *p.id_token.lock().unwrap() = Some(IdToken::default());
    let resp = callback(&app, &flow, "good-code").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "missing nonce");
    assert_eq!(*p.userinfo_hits.lock().unwrap(), 0);
}

/// The GitHub/Forgejo shape: no id_token in the token response, so userinfo is
/// the identity — in either mode. This is the compatibility guarantee.
#[tokio::test]
async fn provider_without_an_id_token_still_logs_in_via_userinfo() {
    for oidc in [false, true] {
        let p = provider(None).await;
        let app = app_for(&p, vec!["alice".into()], oidc);
        let v = body_json(api_login(&app, "good-code").await).await;
        assert_eq!(v["subject"], "alice", "oidc mode: {oidc}");
        assert_eq!(*p.userinfo_hits.lock().unwrap(), 1);
        let session = v["session"].as_str().unwrap();
        assert!(may_write(&app, session).await, "alice is an owner");
    }
}

/// Without a configured issuer there is nothing to verify an id_token against,
/// so plain OAuth2 mode neither trusts nor uses one.
#[tokio::test]
async fn plain_oauth2_mode_ignores_an_id_token() {
    let p = provider(Some(IdToken::default())).await;
    let app = app_for(&p, vec!["8f3c-opaque-uuid".into()], false);

    let v = body_json(api_login(&app, "good-code").await).await;
    assert_eq!(v["subject"], "alice", "userinfo remains the identity");
    assert_eq!(*p.userinfo_hits.lock().unwrap(), 1);
    let session = v["session"].as_str().unwrap();
    assert!(
        !may_write(&app, session).await,
        "the ignored id_token's subject must not confer Owner"
    );
}

// ---------------------------------------------------------------------------
// Owner bootstrap by verified email (ADR-0049 amendment)
// ---------------------------------------------------------------------------

/// An owners entry may be an email, so bootstrapping against Dex/Keycloak/Google
/// does not require discovering opaque per-client UUIDs first.
#[tokio::test]
async fn verified_email_owner_entry_grants_owner() {
    let p = provider(Some(IdToken {
        email: Some("ada@example.com".into()),
        email_verified: Some(serde_json::json!(true)),
        ..Default::default()
    }))
    .await;
    let app = app_for(&p, vec!["ada@example.com".into()], true);

    let v = body_json(api_login(&app, "good-code").await).await;
    // The email is only a MATCHER: the stored identity is still the stable sub.
    assert_eq!(v["subject"], "8f3c-opaque-uuid");
    let session = v["session"].as_str().unwrap();
    assert!(may_write(&app, session).await, "verified email ⇒ Owner");
}

/// …but an unverified (or unasserted) email must never grant Owner — otherwise
/// anyone who can type an email into a profile can administer Scarab.
#[tokio::test]
async fn unverified_email_owner_entry_does_not_grant_owner() {
    for verified in [Some(serde_json::json!(false)), None] {
        let p = provider(Some(IdToken {
            email: Some("ada@example.com".into()),
            email_verified: verified.clone(),
            ..Default::default()
        }))
        .await;
        let app = app_for(&p, vec!["ada@example.com".into()], true);

        let v = body_json(api_login(&app, "good-code").await).await;
        assert_eq!(v["subject"], "8f3c-opaque-uuid");
        let session = v["session"].as_str().unwrap();
        assert!(
            !may_write(&app, session).await,
            "email_verified={verified:?} must log in as Viewer"
        );
    }
}
