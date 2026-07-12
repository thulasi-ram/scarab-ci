//! OIDC issuer acceptance (ADR-0015): a run-scoped RS256 token verifies against
//! Scarab's JWKS with the expected subject/claims, a wrong audience is rejected,
//! rotation keeps old tokens verifying, and the JWKS is served over HTTP.
//! Hermetic — no Postgres.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_identity::{Claims, OidcIssuer};
use scarab_server::oidc::{verify, Rs256Issuer};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

const ISSUER: &str = "https://scarab.example";
const AUD: &str = "sts.amazonaws.com";
// exp far in the future so validation passes without a clock dependency.
const EXP_2100: i64 = 4_102_444_800;

fn run_claims() -> Claims {
    Claims {
        issuer: ISSUER.into(),
        subject: Claims::run_subject("acme", "app", "prod", "refs/heads/main"),
        audience: AUD.into(),
        run_id: "run-1".into(),
        attempt: "a1".into(),
        event: "push".into(),
        git_ref: "refs/heads/main".into(),
        sha: "cafebabe".into(),
        expires_at: EXP_2100,
    }
}

/// Extract `(n, e)` for the JWKS key at `idx`.
fn jwk(issuer: &Rs256Issuer, idx: usize) -> (String, String) {
    let jwks = issuer.jwks();
    let k = &jwks["keys"][idx];
    (
        k["n"].as_str().unwrap().to_string(),
        k["e"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn token_verifies_against_jwks_with_run_subject() {
    let issuer = Rs256Issuer::generate(ISSUER).unwrap();
    let token = issuer.issue(run_claims()).await.unwrap();

    let (n, e) = jwk(&issuer, 0);
    let claims = verify(&token.0, &n, &e, AUD).expect("token verifies against the JWKS");

    assert_eq!(claims["iss"], ISSUER);
    assert_eq!(claims["sub"], "scarab:org/acme/repo/app/env/prod/ref/refs/heads/main");
    assert_eq!(claims["aud"], AUD);
    assert_eq!(claims["run_id"], "run-1");
    assert_eq!(claims["attempt"], "a1");
    assert_eq!(claims["ref"], "refs/heads/main");
    assert_eq!(claims["sha"], "cafebabe");

    // A different audience is rejected.
    assert!(verify(&token.0, &n, &e, "wrong-aud").is_err());
}

#[tokio::test]
async fn rotation_keeps_old_tokens_verifying() {
    let mut issuer = Rs256Issuer::generate(ISSUER).unwrap();
    let old_token = issuer.issue(run_claims()).await.unwrap();
    let (old_n, old_e) = jwk(&issuer, 0);

    // Rotate: a new current key; the JWKS now publishes both.
    issuer.rotate().unwrap();
    assert_eq!(issuer.jwks()["keys"].as_array().unwrap().len(), 2);

    // A token minted now is signed by the new key...
    let new_token = issuer.issue(run_claims()).await.unwrap();
    let (new_n, new_e) = jwk(&issuer, 1);
    assert!(verify(&new_token.0, &new_n, &new_e, AUD).is_ok(), "new token verifies with new key");
    // ...and the old token still verifies against its (still-published) old key.
    assert!(verify(&old_token.0, &old_n, &old_e, AUD).is_ok(), "old token still verifies");
    // Cross-key checks fail (different keypairs).
    assert!(verify(&new_token.0, &old_n, &old_e, AUD).is_err());
}

#[tokio::test]
async fn jwks_is_served_over_http() {
    let issuer = Arc::new(Rs256Issuer::generate(ISSUER).unwrap());
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()));
    let clock = Arc::new(FakeClock::new(1_000));
    let app = router(AppState::new(db, clock, logs).with_oidc(issuer));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/.well-known/jwks.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["keys"][0]["alg"], "RS256");
    assert_eq!(v["keys"][0]["kty"], "RSA");
}
