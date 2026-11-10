//! Issued API tokens (ADR-0049 amendment), hermetic: the credential a machine
//! can hold.
//!
//! The properties under test are the ones that decide whether this is a safe
//! credential or a permanent skeleton key — it is bounded by its minter at mint
//! time, bounded AGAIN by its owner's live authority on every request, confined
//! to one scope, revocable, expiring, and never readable back after mint.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{Db, RunId};
use scarab_identity::{ApiTokenStore, Binding, BindingOrigin, Principal, RbacStore, Role, Scope};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeForge, InMemoryApiTokens, InMemoryDb, InMemoryObjectStore,
    InMemoryRbac, InMemorySessions,
};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A principal with NO global roles — only scoped bindings decide.
fn scoped_principal(subject: &str) -> Principal {
    Principal {
        subject: subject.into(),
        handle: String::new(),
        display_name: None,
        roles: vec![],
    }
}

struct Harness {
    app: axum::Router,
    db: Arc<InMemoryDb>,
    rbac: Arc<InMemoryRbac>,
    tokens: Arc<InMemoryApiTokens>,
    clock: Arc<FakeClock>,
}

fn harness() -> Harness {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential(
                "root-code",
                Principal {
                    subject: "root".into(),
                    handle: String::new(),
                    display_name: None,
                    roles: vec![Role::Owner], // global bootstrap owner
                },
            )
            .with_credential("amy-code", scoped_principal("amy"))
            .with_credential("eve-code", scoped_principal("eve")),
    );
    let rbac = Arc::new(InMemoryRbac::new());
    let tokens = Arc::new(InMemoryApiTokens::new());
    let app = router(
        AppState::new(db.clone(), clock.clone(), logs)
            .with_auth(auth, Arc::new(InMemorySessions::new()))
            .with_rbac(rbac.clone())
            .with_api_tokens(tokens.clone())
            .with_forge(Arc::new(FakeForge::new())),
    );
    Harness {
        app,
        db,
        rbac,
        tokens,
        clock,
    }
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn login(app: &axum::Router, credential: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "credential": credential }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["session"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn grant(h: &Harness, subject: &str, scope: Scope, role: Role) {
    h.rbac
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

/// Log in as the global bootstrap owner AND give them a native Owner binding on
/// `org`.
///
/// The binding is not ceremony: a token's authority is re-derived on every
/// request from the sources a token can see — the `owners` config and the
/// native bindings — never from the login-time snapshot on the minter's
/// session. With no OAuth provider wired here, `rbac_bindings` is the only one
/// of the two that exists, so this is what a mintable authority looks like in
/// this harness.
async fn root_owning(h: &Harness, org: &str) -> String {
    let session = login(&h.app, "root-code").await;
    grant(h, "root", Scope::Org(org.into()), Role::Owner).await;
    session
}

fn get(uri: &str, bearer: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap()
}

fn mint_req(bearer: &str, org: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/orgs/{org}/tokens"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Mint a token and return `(plaintext, id)`, asserting a 201.
async fn mint(h: &Harness, bearer: &str, org: &str, body: serde_json::Value) -> (String, String) {
    let resp = h
        .app
        .clone()
        .oneshot(mint_req(bearer, org, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp).await;
    (
        v["token"].as_str().unwrap().to_string(),
        v["record"]["id"].as_str().unwrap().to_string(),
    )
}

/// Create a run as the global owner, then stamp its tenant.
async fn seed_run(h: &Harness, root: &str, org: &str, project: &str) -> String {
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {root}"))
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
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = body_json(resp).await["id"].as_str().unwrap().to_string();
    h.db.set_run_tenant(&RunId(id.clone()), org, project)
        .await
        .unwrap();
    id
}

// ---------------------------------------------------------------------------

/// The whole point: a token is a bearer credential a machine can hold, and it
/// works on the same Bearer path `scarab-cli --token` already sends on.
#[tokio::test]
async fn a_minted_token_authenticates_as_a_bearer_credential() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    let run = seed_run(&h, &root, "acme", "app").await;

    let (token, _) = mint(
        &h,
        &root,
        "acme",
        serde_json::json!({ "name": "ci", "role": "viewer", "expires_in_days": 30 }),
    )
    .await;

    assert!(
        token.starts_with("scarab_pat_"),
        "the prefix is what routes it and what a secret scanner matches: {token}"
    );

    let resp = h
        .app
        .clone()
        .oneshot(get(&format!("/v1/runs/{run}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Revocation and expiry each kill a token on their own, and both answer the
/// same 401 an unknown string does — telling them apart would make this an
/// oracle for which tokens exist.
#[tokio::test]
async fn a_revoked_or_expired_token_no_longer_authenticates() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    let run = seed_run(&h, &root, "acme", "app").await;

    let (revoked, revoked_id) = mint(
        &h,
        &root,
        "acme",
        serde_json::json!({ "name": "doomed", "role": "viewer", "expires_in_days": 30 }),
    )
    .await;
    let (expiring, _) = mint(
        &h,
        &root,
        "acme",
        serde_json::json!({ "name": "short", "role": "viewer", "expires_in_days": 1 }),
    )
    .await;

    // Both work right now.
    for t in [&revoked, &expiring] {
        let resp = h
            .app
            .clone()
            .oneshot(get(&format!("/v1/runs/{run}"), t))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Revoke one through the API.
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/orgs/acme/tokens/{revoked_id}"))
                .header("authorization", format!("Bearer {root}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = h
        .app
        .clone()
        .oneshot(get(&format!("/v1/runs/{run}"), &revoked))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "revocation is effective on the very next request"
    );

    // Let the other one age out. Only the clock is mocked (CONTEXT.md §8).
    h.clock.advance(DAY_MS + 1);
    let resp = h
        .app
        .clone()
        .oneshot(get(&format!("/v1/runs/{run}"), &expiring))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // An unknown string is indistinguishable from either.
    let resp = h
        .app
        .clone()
        .oneshot(get(
            &format!("/v1/runs/{run}"),
            "scarab_pat_nothingwaseverhere",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// The decision most worth not deferring: a token carries an explicit role
/// SUBSET of its minter's, never "inherit whatever the minter has, forever".
#[tokio::test]
async fn a_token_cannot_exceed_its_minters_authority_at_mint_time() {
    let h = harness();
    // amy administers acme/app and nothing else.
    grant(
        &h,
        "amy",
        Scope::Project {
            org: "acme".into(),
            name: "app".into(),
        },
        Role::Admin,
    )
    .await;
    let amy = login(&h.app, "amy-code").await;

    // At or below her own role: fine.
    for role in ["viewer", "member", "admin"] {
        let resp = h
            .app
            .clone()
            .oneshot(mint_req(
                &amy,
                "acme",
                serde_json::json!({
                    "name": format!("as-{role}"),
                    "project": "app",
                    "role": role,
                    "expires_in_days": 7
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "minting `{role}`");
    }

    // Above it: refused, and the message says what she actually holds.
    let resp = h
        .app
        .clone()
        .oneshot(mint_req(
            &amy,
            "acme",
            serde_json::json!({
                "name": "escalate",
                "project": "app",
                "role": "owner",
                "expires_in_days": 7
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let msg = body_text(resp).await;
    assert!(msg.contains("may not exceed its minter"), "{msg}");

    // And she cannot mint into a scope where she holds no role AT ALL — the
    // org above her project included (ADR-0049: Org inherits down, never up).
    for body in [
        serde_json::json!({ "name": "x", "role": "viewer", "expires_in_days": 7 }),
        serde_json::json!({ "name": "x", "project": "other", "role": "viewer", "expires_in_days": 7 }),
    ] {
        let resp = h
            .app
            .clone()
            .oneshot(mint_req(&amy, "acme", body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}

/// The ADR-0049 cross-tenant leak, re-asserted on this credential: a token
/// scoped to one Project cannot read another's runs — even when its OWNER can.
#[tokio::test]
async fn a_project_scoped_token_cannot_read_another_projects_runs() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    let app_run = seed_run(&h, &root, "acme", "app").await;
    let api_run = seed_run(&h, &root, "acme", "api").await;

    // amy administers the whole org, so she can read both runs herself.
    grant(&h, "amy", Scope::Org("acme".into()), Role::Admin).await;
    let amy = login(&h.app, "amy-code").await;
    for run in [&app_run, &api_run] {
        let resp = h
            .app
            .clone()
            .oneshot(get(&format!("/v1/runs/{run}"), &amy))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "amy herself reads {run}");
    }

    // Her token, scoped to acme/app, reads exactly one of them.
    let (token, _) = mint(
        &h,
        &amy,
        "acme",
        serde_json::json!({
            "name": "app only",
            "project": "app",
            "role": "viewer",
            "expires_in_days": 7
        }),
    )
    .await;

    let resp = h
        .app
        .clone()
        .oneshot(get(&format!("/v1/runs/{app_run}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = h
        .app
        .clone()
        .oneshot(get(&format!("/v1/runs/{api_run}"), &token))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "the token is narrower than its owner"
    );

    // The listing agrees with the per-run answer — a filter that forgot the
    // grant would leak the sibling's existence without ever 403-ing.
    let resp = h
        .app
        .clone()
        .oneshot(get("/v1/runs", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listed: Vec<String> = body_json(resp).await["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listed, vec![app_run]);
}

/// The live half of the cap: the token row is a CEILING, not a snapshot of
/// authority. Demote the owner and the token demotes with them, in the same
/// instant, with no list to walk.
#[tokio::test]
async fn a_token_loses_power_the_moment_its_owner_does() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    let run = seed_run(&h, &root, "acme", "app").await;

    grant(&h, "amy", Scope::Org("acme".into()), Role::Admin).await;
    let amy = login(&h.app, "amy-code").await;
    let (token, _) = mint(
        &h,
        &amy,
        "acme",
        serde_json::json!({ "name": "ci", "role": "admin", "expires_in_days": 30 }),
    )
    .await;

    // An Administer surface the token reaches while amy is an org Admin.
    let resp = h
        .app
        .clone()
        .oneshot(get("/v1/orgs/acme/bindings", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // amy is demoted to Viewer. The token's own row is untouched.
    grant(&h, "amy", Scope::Org("acme".into()), Role::Viewer).await;

    let resp = h
        .app
        .clone()
        .oneshot(get("/v1/orgs/acme/bindings", &token))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "the ceiling is still `admin`, but the live role is not"
    );
    // Read still works — it demoted, it did not die.
    let resp = h
        .app
        .clone()
        .oneshot(get(&format!("/v1/runs/{run}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Offboard her entirely and the token holds nothing at all.
    h.rbac
        .revoke("amy", &Scope::Org("acme".into()))
        .await
        .unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(get(&format!("/v1/runs/{run}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// A token may not mint tokens. Without this, the mandatory expiry bounds
/// nothing: a token holding Administer could mint its own successor forever.
#[tokio::test]
async fn a_token_may_not_mint_another_token() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    let (token, _) = mint(
        &h,
        &root,
        "acme",
        serde_json::json!({ "name": "powerful", "role": "owner", "expires_in_days": 30 }),
    )
    .await;

    // It really does hold Administer here — the refusal below is about WHO is
    // asking, not about what they hold.
    let resp = h
        .app
        .clone()
        .oneshot(get("/v1/orgs/acme/tokens", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = h
        .app
        .clone()
        .oneshot(mint_req(
            &token,
            "acme",
            serde_json::json!({ "name": "successor", "role": "owner", "expires_in_days": 30 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// The plaintext exists once, in the mint response. It is not in the listing,
/// not in the store, and cannot be recovered from either.
#[tokio::test]
async fn the_plaintext_appears_once_and_is_never_stored() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    let (token, id) = mint(
        &h,
        &root,
        "acme",
        serde_json::json!({ "name": "ci", "role": "member", "expires_in_days": 30 }),
    )
    .await;

    let resp = h
        .app
        .clone()
        .oneshot(get("/v1/orgs/acme/tokens", &root))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listing = body_text(resp).await;
    assert!(listing.contains(&id), "the record is listed: {listing}");
    assert!(
        !listing.contains(&token),
        "the plaintext must never come back: {listing}"
    );

    // Nor is it in the store: the record is reachable only by the HASH of the
    // plaintext, so a database dump carries no usable credential.
    assert!(h.tokens.by_hash(&token).await.unwrap().is_none());
    let stored = h
        .tokens
        .by_hash(&scarab_identity::api_token_hash(&token))
        .await
        .unwrap()
        .expect("stored under its digest");
    assert_eq!(stored.id, id);
    assert_eq!(stored.owner_subject, "root");
}

/// Guards the `authenticate()` branch the whole design leans on: a Bearer
/// caller presents its credential explicitly, so there is no CSRF surface and
/// no `x-csrf-token` to demand. A cookie-borne token, conversely, is refused —
/// honouring one would put it back on the ambient path CSRF exists to defend.
#[tokio::test]
async fn a_token_needs_no_csrf_header_but_is_refused_from_a_cookie() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    let (token, _) = mint(
        &h,
        &root,
        "acme",
        serde_json::json!({ "name": "ci", "role": "admin", "expires_in_days": 30 }),
    )
    .await;

    // A MUTATION over Bearer, with no CSRF header anywhere.
    let put = |auth: (&str, String)| {
        Request::builder()
            .method("PUT")
            .uri("/v1/orgs/acme/bindings")
            .header("content-type", "application/json")
            .header(auth.0, auth.1)
            .body(Body::from(
                serde_json::json!({ "subject": "eve", "role": "viewer" }).to_string(),
            ))
            .unwrap()
    };
    let resp = h
        .app
        .clone()
        .oneshot(put(("authorization", format!("Bearer {token}"))))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // The same token in the session cookie is not a credential at all.
    let resp = h
        .app
        .clone()
        .oneshot(put(("cookie", format!("scarab_session={token}"))))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A token nobody can see the last use of is a token nobody will ever dare
/// revoke — so use is recorded, coarsely.
#[tokio::test]
async fn a_tokens_last_use_is_recorded() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    let run = seed_run(&h, &root, "acme", "app").await;
    let (token, _) = mint(
        &h,
        &root,
        "acme",
        serde_json::json!({ "name": "ci", "role": "viewer", "expires_in_days": 30 }),
    )
    .await;

    let hash = scarab_identity::api_token_hash(&token);
    assert_eq!(
        h.tokens.by_hash(&hash).await.unwrap().unwrap().last_used_at,
        None
    );

    h.clock.advance(5_000);
    let resp = h
        .app
        .clone()
        .oneshot(get(&format!("/v1/runs/{run}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        h.tokens.by_hash(&hash).await.unwrap().unwrap().last_used_at,
        Some(6_000)
    );
}

/// An expiry is mandatory and bounded, and the name must identify the thing —
/// the two fields that make a token revocable in practice rather than in
/// principle.
#[tokio::test]
async fn a_token_needs_a_name_and_a_bounded_expiry() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    for (label, body) in [
        (
            "no lifetime",
            serde_json::json!({ "name": "x", "role": "viewer", "expires_in_days": 0 }),
        ),
        (
            "past the ceiling",
            serde_json::json!({ "name": "x", "role": "viewer", "expires_in_days": 366 }),
        ),
        (
            "blank name",
            serde_json::json!({ "name": "   ", "role": "viewer", "expires_in_days": 7 }),
        ),
        (
            "unknown role",
            serde_json::json!({ "name": "x", "role": "superuser", "expires_in_days": 7 }),
        ),
    ] {
        let resp = h
            .app
            .clone()
            .oneshot(mint_req(&root, "acme", body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{label}");
    }
}

/// Revoking is scoped by org: `id` alone is a global handle, and an admin of
/// one org must not reach into another's by guessing one.
#[tokio::test]
async fn a_token_can_only_be_revoked_from_the_org_that_issued_it() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    let (_, id) = mint(
        &h,
        &root,
        "acme",
        serde_json::json!({ "name": "ci", "role": "viewer", "expires_in_days": 30 }),
    )
    .await;

    // evil's admin knows the id but cannot spend it.
    grant(&h, "eve", Scope::Org("evil".into()), Role::Admin).await;
    let eve = login(&h.app, "eve-code").await;
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/orgs/evil/tokens/{id}"))
                .header("authorization", format!("Bearer {eve}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // And a second revoke by the rightful admin reports NOT FOUND rather than
    // quietly rewriting when the credential actually died.
    for expected in [StatusCode::NO_CONTENT, StatusCode::NOT_FOUND] {
        let resp = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/orgs/acme/tokens/{id}"))
                    .header("authorization", format!("Bearer {root}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), expected);
    }
}

// ---------------------------------------------------------------------------
// Personal tokens (git-bug a4cb94f): a person mints and revokes their own
// under `/v1/me/tokens`; the org lists every token issued in it.

fn send(method: &str, uri: &str, bearer: &str, body: Option<serde_json::Value>) -> Request<Body> {
    let b = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"));
    match body {
        Some(body) => b
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    }
}

async fn call(h: &Harness, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Mint through `POST /v1/me/tokens`, returning `(plaintext, id)`.
async fn mint_own(h: &Harness, bearer: &str, body: serde_json::Value) -> (String, String) {
    let (st, v) = call(h, send("POST", "/v1/me/tokens", bearer, Some(body))).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    (
        v["token"].as_str().unwrap().to_string(),
        v["record"]["id"].as_str().unwrap().to_string(),
    )
}

fn names(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// A member issues a token for herself, up to her own role and no further,
/// and only in an org where she holds a role. Before a4cb94f only an admin
/// could mint at all.
#[tokio::test]
async fn a_member_mints_her_own_token_up_to_her_role_and_no_higher() {
    let h = harness();
    grant(&h, "amy", Scope::Org("acme".into()), Role::Member).await;
    let amy = login(&h.app, "amy-code").await;

    for role in ["viewer", "member"] {
        let (token, _) = mint_own(
            &h,
            &amy,
            serde_json::json!({ "org": "acme", "name": role, "role": role, "expires_in_days": 7 }),
        )
        .await;
        assert!(token.starts_with("scarab_pat_"));
    }
    let (st, v) = call(
        &h,
        send(
            "POST",
            "/v1/me/tokens",
            &amy,
            Some(serde_json::json!({ "org": "acme", "name": "up", "role": "admin", "expires_in_days": 7 })),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");

    // The org route is the same mint: a member may use it too, with the same
    // ceiling.
    let (st, v) = call(
        &h,
        mint_req(
            &amy,
            "acme",
            serde_json::json!({ "name": "up", "role": "admin", "expires_in_days": 7 }),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");

    // An org where she holds nothing: refused outright.
    let (st, _) = call(
        &h,
        send(
            "POST",
            "/v1/me/tokens",
            &amy,
            Some(serde_json::json!({ "org": "evil", "name": "x", "role": "viewer", "expires_in_days": 7 })),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // Every token she minted is hers.
    let owners: Vec<String> = h
        .tokens
        .list("acme")
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.owner_subject)
        .collect();
    assert_eq!(owners, vec!["amy", "amy"]);
}

/// `/v1/me/tokens` lists the caller's own tokens and no one else's, and its
/// revoke reaches only those: someone else's id answers 404, like a missing
/// one.
#[tokio::test]
async fn my_tokens_are_only_mine() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    grant(&h, "amy", Scope::Org("acme".into()), Role::Member).await;
    let amy = login(&h.app, "amy-code").await;

    let (_, roots) = mint_own(
        &h,
        &root,
        serde_json::json!({ "org": "acme", "name": "root-ci", "role": "viewer", "expires_in_days": 7 }),
    )
    .await;
    let (_, amys) = mint_own(
        &h,
        &amy,
        serde_json::json!({ "org": "acme", "name": "amy-laptop", "role": "viewer", "expires_in_days": 7 }),
    )
    .await;

    let (st, v) = call(&h, send("GET", "/v1/me/tokens", &amy, None)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(names(&v), vec!["amy-laptop"]);
    let (_, v) = call(&h, send("GET", "/v1/me/tokens", &root, None)).await;
    assert_eq!(names(&v), vec!["root-ci"]);

    let (st, _) = call(
        &h,
        send("DELETE", &format!("/v1/me/tokens/{roots}"), &amy, None),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(h
        .tokens
        .by_id(&roots)
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_none());

    let (st, _) = call(
        &h,
        send("DELETE", &format!("/v1/me/tokens/{amys}"), &amy, None),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(h
        .tokens
        .by_id(&amys)
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
}

/// Every call is counted, not one per minute: the in-memory count is written
/// when a token list is read, and directly once the stored last use is a
/// minute old.
#[tokio::test]
async fn a_tokens_calls_are_counted() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    let (token, id) = mint_own(
        &h,
        &root,
        serde_json::json!({ "org": "acme", "name": "ci", "role": "viewer", "expires_in_days": 7 }),
    )
    .await;
    let stored = |h: &Harness| {
        let tokens = h.tokens.clone();
        let id = id.clone();
        async move { tokens.by_id(&id).await.unwrap().unwrap().use_count }
    };

    // Three calls inside one minute: the first is written at once (no last
    // use yet), the other two wait in memory.
    for _ in 0..3 {
        h.clock.advance(1_000);
        let (st, _) = call(&h, get("/v1/repos", &token)).await;
        assert!(st.is_success(), "{st}");
    }
    assert_eq!(stored(&h).await, 1);

    // Reading the org's list writes them first.
    let (st, v) = call(&h, get("/v1/orgs/acme/tokens", &root)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v[0]["use_count"], 3, "{v}");
    assert_eq!(stored(&h).await, 3);

    // A minute later a call is written as it happens.
    h.clock.advance(61_000);
    let (st, _) = call(&h, get("/v1/repos", &token)).await;
    assert!(st.is_success(), "{st}");
    assert_eq!(stored(&h).await, 4);
}

/// The org's list holds every token issued in it, whoever owns it, and none
/// from another org; it sorts by last use (the default), by calls, or by
/// creation.
#[tokio::test]
async fn the_org_list_sorts_by_last_use_calls_or_creation() {
    let h = harness();
    let root = root_owning(&h, "acme").await;
    grant(&h, "root", Scope::Org("evil".into()), Role::Owner).await;
    grant(&h, "amy", Scope::Org("acme".into()), Role::Member).await;
    let amy = login(&h.app, "amy-code").await;

    let body = |name: &str| serde_json::json!({ "org": "acme", "name": name, "role": "viewer", "expires_in_days": 7 });
    let (a, _) = mint_own(&h, &root, body("a")).await;
    h.clock.advance(1_000);
    let (b, _) = mint_own(&h, &amy, body("b")).await;
    h.clock.advance(1_000);
    mint_own(&h, &root, body("c")).await;
    // Another org's token is never listed in acme's.
    mint_own(
        &h,
        &root,
        serde_json::json!({ "org": "evil", "name": "elsewhere", "role": "viewer", "expires_in_days": 7 }),
    )
    .await;

    // a: three calls, first. b: one call, later. c: never used.
    h.clock.advance(1_000);
    for _ in 0..3 {
        assert!(call(&h, get("/v1/repos", &a)).await.0.is_success());
    }
    h.clock.advance(1_000);
    assert!(call(&h, get("/v1/repos", &b)).await.0.is_success());

    for (query, want) in [
        ("", vec!["b", "a", "c"]),
        ("?sort=last_used", vec!["b", "a", "c"]),
        ("?sort=use_count", vec!["a", "b", "c"]),
        ("?sort=created", vec!["c", "b", "a"]),
    ] {
        let (st, v) = call(&h, get(&format!("/v1/orgs/acme/tokens{query}"), &root)).await;
        assert_eq!(st, StatusCode::OK, "{query}: {v}");
        assert_eq!(names(&v), want, "{query}");
    }
    let (_, v) = call(&h, get("/v1/orgs/acme/tokens", &root)).await;
    let owners: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["owner_subject"].as_str().unwrap())
        .collect();
    assert_eq!(owners, vec!["amy", "root", "root"]);

    let (st, _) = call(&h, get("/v1/orgs/acme/tokens?sort=bogus", &root)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // A member does not see the org's list.
    let (st, _) = call(&h, get("/v1/orgs/acme/tokens", &amy)).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

fn sorted_names(v: &serde_json::Value) -> Vec<String> {
    let mut n = names(v);
    n.sort();
    n
}

/// A token on `/v1/me/tokens` lists only the tokens inside its own scope and
/// may neither mint nor revoke: the personal revoke checks ownership and
/// nothing else, so a leaked viewer token must not be able to revoke its
/// owner's admin token. The owner's session can.
#[tokio::test]
async fn a_token_lists_within_its_scope_and_may_not_mint_or_revoke() {
    let h = harness();
    grant(&h, "amy", Scope::Org("acme".into()), Role::Admin).await;
    grant(&h, "amy", Scope::Org("globex".into()), Role::Member).await;
    let amy = login(&h.app, "amy-code").await;
    let body = |org: &str, name: &str, role: &str, project: Option<&str>| {
        let mut b =
            serde_json::json!({ "org": org, "name": name, "role": role, "expires_in_days": 7 });
        if let Some(p) = project {
            b["project"] = serde_json::json!(p);
        }
        b
    };
    let (poller, poller_id) = mint_own(&h, &amy, body("acme", "poller", "viewer", None)).await;
    let (_, deploy_id) = mint_own(&h, &amy, body("acme", "deploy", "admin", None)).await;
    let (web, _) = mint_own(&h, &amy, body("acme", "web-only", "viewer", Some("web"))).await;
    mint_own(&h, &amy, body("globex", "other", "member", None)).await;

    // Listing: an org token sees its org's tokens, project ones included; a
    // project token sees only its project's.
    let (st, v) = call(&h, send("GET", "/v1/me/tokens", &poller, None)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(sorted_names(&v), vec!["deploy", "poller", "web-only"]);
    let (st, v) = call(&h, send("GET", "/v1/me/tokens", &web, None)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(names(&v), vec!["web-only"]);

    // Minting: refused for a token, whatever it asks for.
    let (st, _) = call(
        &h,
        send(
            "POST",
            "/v1/me/tokens",
            &poller,
            Some(body("acme", "next", "viewer", None)),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // Revoking: refused for a token, a sibling's id and its own alike.
    for id in [&deploy_id, &poller_id] {
        let (st, _) = call(
            &h,
            send("DELETE", &format!("/v1/me/tokens/{id}"), &poller, None),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{id}");
        assert!(h
            .tokens
            .by_id(id)
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_none());
    }

    // The owner's session revokes it.
    let (st, _) = call(
        &h,
        send("DELETE", &format!("/v1/me/tokens/{deploy_id}"), &amy, None),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(h
        .tokens
        .by_id(&deploy_id)
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
}

/// Anyone with a role in a scope may mint there, a viewer included, up to
/// that role and no further.
#[tokio::test]
async fn a_viewer_mints_a_viewer_token_and_nothing_higher() {
    let h = harness();
    grant(&h, "eve", Scope::Org("acme".into()), Role::Viewer).await;
    let eve = login(&h.app, "eve-code").await;

    let (token, _) = mint_own(
        &h,
        &eve,
        serde_json::json!({ "org": "acme", "name": "status", "role": "viewer", "expires_in_days": 7 }),
    )
    .await;
    assert!(token.starts_with("scarab_pat_"));
    let (st, v) = call(
        &h,
        send(
            "POST",
            "/v1/me/tokens",
            &eve,
            Some(serde_json::json!({ "org": "acme", "name": "up", "role": "member", "expires_in_days": 7 })),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");

    // `/v1/me` offers acme to mint in.
    let (st, v) = call(&h, get("/v1/me", &eve)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["token_mint_orgs"], serde_json::json!(["acme"]));
}

/// An Admin of one project mints an admin token for that project, and
/// nothing for the org around it: Org inherits down, never up.
#[tokio::test]
async fn a_project_admin_mints_for_the_project_but_not_the_org() {
    let h = harness();
    grant(
        &h,
        "amy",
        Scope::Project {
            org: "acme".into(),
            name: "web".into(),
        },
        Role::Admin,
    )
    .await;
    let amy = login(&h.app, "amy-code").await;

    let (st, v) = call(
        &h,
        send(
            "POST",
            "/v1/me/tokens",
            &amy,
            Some(serde_json::json!({ "org": "acme", "project": "web", "name": "web-deploy", "role": "admin", "expires_in_days": 7 })),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["record"]["project"], "web");

    for role in ["admin", "viewer"] {
        let (st, v) = call(
            &h,
            send(
                "POST",
                "/v1/me/tokens",
                &amy,
                Some(serde_json::json!({ "org": "acme", "name": "org-wide", "role": role, "expires_in_days": 7 })),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{role}: {v}");
    }

    // The org of her project is offered; she holds nothing anywhere else.
    let (_, v) = call(&h, get("/v1/me", &amy)).await;
    assert_eq!(v["token_mint_orgs"], serde_json::json!(["acme"]));
    let none = login(&h.app, "eve-code").await;
    let (_, v) = call(&h, get("/v1/me", &none)).await;
    assert_eq!(v["token_mint_orgs"], serde_json::json!([]));
}
