//! Feature acceptance for declarative (IaC) forge connections — ADR-0060 part D.
//!
//! The user-visible claims under test, in the ticket's own words:
//!
//!  1. **A config-declared Forgejo connection boots, resolves its credential
//!     from an env var, and is read-only via the API.** The secret store is
//!     deliberately left EMPTY: if the connection reports a healthy credential
//!     anyway, the env-override leg of the one resolution path is genuinely wired
//!     — not shadowed by a `SecretProvider` lookup that happened to succeed.
//!  2. **A connection is owned by exactly one source.** Declaring in config an id
//!     the database already owns refuses the boot instead of picking a winner,
//!     and undeclaring *releases* ownership rather than deleting a connection
//!     (Projects, Environments, secrets and RBAC hang off its repo bindings).
//!  3. **Credential resolution is one path**: deployment override →
//!     `SecretProvider`. Where both exist, the override wins, for GitHub and
//!     Forgejo alike.
//!
//! Hermetic: `InMemoryDb` doubles as the connection registry (it mirrors the
//! Postgres `owned_by_config` column), `FakeSecrets` as the secret store. The
//! YAML→specs step runs through the real config parser with an injected
//! environment, so the block under test is the one an operator would write.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_identity::{Principal, Role};
use scarab_secrets::{Secret, SecretProvider, SecretScope};
use scarab_server::config::{connections_from_env, ConnectionSpec};
use scarab_server::connections_config::{
    provision, resolve_connection_credential, CredentialOverrides, ProvisionError,
};
use scarab_server::{router, AppState, LogService, FORGE_CREDENTIALS_ORG};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeForge, FakeSecrets, InMemoryDb, InMemoryObjectStore,
    InMemoryRbac, InMemorySessions,
};

/// What an operator writes in Helm values / a mounted ConfigMap: a self-hosted
/// Forgejo whose token is delivered by the deployment, plus the repos it owns
/// (each binding *is* a Project, ADR-0046).
const BLOCK: &str = r#"
connections:
  - id: forgejo-main
    kind: forgejo
    base_url: https://git.example.com
    credential:
      env: FORGEJO_CI_TOKEN
    repos:
      - acme/widgets
"#;

/// The declarative block as the config gate resolves it, with the credential
/// env var supplied by the deployment.
fn specs_from_block(block: &'static str) -> Vec<ConnectionSpec> {
    connections_from_env(&move |k: &str| match k {
        "SCARAB_CONNECTIONS" => Some(block.to_string()),
        "FORGEJO_CI_TOKEN" => Some("tok-from-env".to_string()),
        _ => None,
    })
    .expect("the block is valid")
}

/// An API stack over `db`, with auth on (an Owner token) and an empty secret
/// store unless `seed` says otherwise.
async fn app(
    db: Arc<InMemoryDb>,
    overrides: CredentialOverrides,
    seeded: Option<&str>,
) -> axum::Router {
    let secrets = Arc::new(FakeSecrets::new());
    if let Some(value) = seeded {
        secrets
            .put(
                &SecretScope::Org {
                    org: FORGE_CREDENTIALS_ORG.into(),
                },
                Secret {
                    key: "forgejo-main".into(),
                    value: value.as_bytes().to_vec(),
                },
            )
            .await
            .unwrap();
    }
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let auth = Arc::new(FakeAuthenticator::new().with_credential(
        "root-code",
        Principal {
            subject: "root".into(),
            display_name: None,
            roles: vec![Role::Owner],
        },
    ));
    router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(0)), logs)
            .with_auth(auth, Arc::new(InMemorySessions::new()))
            .with_rbac(Arc::new(InMemoryRbac::new()))
            .with_secrets(secrets)
            .with_forge(Arc::new(FakeForge::new()))
            .with_forge_connections(db)
            .with_credential_overrides(Arc::new(overrides)),
    )
}

async fn login(app: &axum::Router) -> String {
    let res = app
        .clone()
        .oneshot(
            Request::post("/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"credential":"root-code"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    body["session"]
        .as_str()
        .expect("a session token")
        .to_string()
}

async fn list(app: &axum::Router, token: &str) -> serde_json::Value {
    let res = app
        .clone()
        .oneshot(
            Request::get("/v1/connections")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(res.into_body(), 256 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

/// The ticket's acceptance criterion, end to end.
#[tokio::test]
async fn a_config_declared_forgejo_connection_is_provisioned_healthy_and_read_only() {
    let specs = specs_from_block(BLOCK);
    let db = Arc::new(InMemoryDb::new());

    let provisioned = provision(db.as_ref(), &specs).await.unwrap();
    assert_eq!(provisioned.owned, vec!["forgejo-main".to_string()]);
    assert_eq!(provisioned.bound, vec!["acme/widgets".to_string()]);

    // It is a real registry row, so every existing consumer (forge router,
    // clone-step enricher, webhook resolution) sees it with no special case.
    let resolved = db
        .resolve(&RepoRef {
            owner: "acme".into(),
            name: "widgets".into(),
        })
        .await
        .unwrap()
        .expect("the config-declared repo resolves to a Project");
    assert_eq!(resolved.connection.id, "forgejo-main");
    assert_eq!(
        (resolved.org.as_str(), resolved.project.as_str()),
        ("acme", "widgets")
    );

    // Note the empty secret store: nothing but the env override can make this
    // credential report healthy.
    let app = app(db.clone(), CredentialOverrides::from_specs(&specs), None).await;
    let token = login(&app).await;
    let body = list(&app, &token).await;
    let c = &body[0];
    assert_eq!(c["id"], "forgejo-main");
    assert_eq!(c["kind"], "forgejo");
    assert_eq!(c["base_url"], "https://git.example.com");
    // Read-only, and labelled as such.
    assert_eq!(c["managed_by_config"], true);
    // Credential health tells the truth about a config-supplied credential…
    assert_eq!(c["credential_present"], true);
    // …without ever echoing the material.
    assert!(!body.to_string().contains("tok-from-env"), "{body}");
    // No mutating affordance is offered.
    assert_eq!(c["supports_resync"], false);
    // The declared repo shows up as the Project it created.
    assert_eq!(c["projects"][0]["org"], "acme");
    assert_eq!(c["projects"][0]["project"], "widgets");

    // …and the one mutating endpoint that exists refuses it outright, rather
    // than writing a binding whose home is the config.
    let res = app
        .clone()
        .oneshot(
            Request::post("/v1/connections/forgejo-main/resync")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let msg = String::from_utf8(
        axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(msg.contains("managed by configuration"), "{msg}");
}

/// **Every** mutating endpoint refuses a config-owned connection — not just the
/// one that existed when part D was written.
///
/// Slices 4/5 added connection create/delete and repo bind/unbind/webhook in
/// parallel with slice 6, so each arrived without the ownership guard. That is
/// the integration hazard worth a test rather than a code review: a UI write
/// that lands on a config-owned row is either reverted by the next deploy or
/// silently diverges from the source that is authoritative for it. Read paths
/// stay open, because seeing the connection is how an admin confirms the deploy
/// took effect.
#[tokio::test]
async fn every_mutating_endpoint_refuses_a_config_owned_connection() {
    let specs = specs_from_block(BLOCK);
    let db = Arc::new(InMemoryDb::new());
    let provisioned = provision(db.as_ref(), &specs).await.unwrap();
    assert_eq!(provisioned.owned, vec!["forgejo-main".to_string()]);
    let app = app(db.clone(), CredentialOverrides::from_specs(&specs), None).await;
    let token = login(&app).await;

    let bearer = |r: axum::http::request::Builder| {
        r.header("authorization", format!("Bearer {token}"))
    };
    // Each of these would write to a row whose contents the `connections:` block
    // owns. `available-repos` is deliberately absent: it only reads.
    let writes: Vec<(&str, Request<Body>)> = vec![
        (
            "delete connection",
            bearer(Request::delete("/v1/connections/forgejo-main"))
                .body(Body::empty())
                .unwrap(),
        ),
        (
            "bind repo",
            bearer(Request::post("/v1/connections/forgejo-main/repos"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "owner": "acme", "name": "extra" }).to_string(),
                ))
                .unwrap(),
        ),
        (
            "unbind repo",
            bearer(Request::delete(
                "/v1/connections/forgejo-main/repos/acme/widgets",
            ))
            .body(Body::empty())
            .unwrap(),
        ),
        (
            "register webhook",
            bearer(Request::post(
                "/v1/connections/forgejo-main/repos/acme/widgets/webhook",
            ))
            .body(Body::empty())
            .unwrap(),
        ),
    ];

    for (what, req) in writes {
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::CONFLICT,
            "{what} must be refused on a config-owned connection"
        );
        let msg = String::from_utf8(
            axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        // The refusal has to say where the change belongs, or the admin is stuck.
        assert!(
            msg.contains("managed by configuration") && msg.contains("connections:"),
            "{what}: refusal must name the config block — got {msg}"
        );
    }

    // The declared binding survived every attempt above.
    let body = list(&app, &token).await;
    assert_eq!(body[0]["projects"][0]["project"], "widgets");

    // And a READ of the pick-list is still allowed.
    let res = app
        .clone()
        .oneshot(
            bearer(Request::get(
                "/v1/connections/forgejo-main/available-repos",
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        res.status(),
        StatusCode::CONFLICT,
        "reading a config-owned connection must stay allowed"
    );
}

/// A DB-owned connection is never silently taken over: two owners is the drift
/// hazard part D exists to remove, so the boot refuses and says which is which.
#[tokio::test]
async fn declaring_a_database_owned_connection_in_config_refuses_the_boot() {
    let db = Arc::new(InMemoryDb::new());
    // As the UI (or the GitHub installation webhook) would have created it.
    db.put_connection(&ForgeConnection {
        id: "forgejo-main".into(),
        kind: ForgeKind::Forgejo,
        base_url: "https://typed-by-hand.example.com".into(),
        credential_ref: "hand-made".into(),
    })
    .await
    .unwrap();

    let err = provision(db.as_ref(), &specs_from_block(BLOCK))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        ProvisionError::OwnershipCollision {
            id: "forgejo-main".into()
        }
    );
    let msg = err.to_string();
    assert!(msg.contains("exactly ONE owner"), "{msg}");

    // Refused BEFORE any write: the existing connection is untouched, so a
    // failed boot cannot leave a half-applied registry behind.
    let conn = db.get_connection("forgejo-main").await.unwrap().unwrap();
    assert_eq!(conn.base_url, "https://typed-by-hand.example.com");
    assert_eq!(conn.credential_ref, "hand-made");
    assert!(db.config_owned_connection_ids().await.unwrap().is_empty());
}

/// Restarts must be boring, and config must stay authoritative: re-provisioning
/// the same block is a no-op, and an edited `base_url` in config wins over the
/// row it wrote last time.
#[tokio::test]
async fn re_provisioning_is_idempotent_and_config_stays_authoritative() {
    let db = Arc::new(InMemoryDb::new());
    let specs = specs_from_block(BLOCK);
    provision(db.as_ref(), &specs).await.unwrap();
    // Second boot: same block, no collision with the row the first boot wrote.
    provision(db.as_ref(), &specs).await.unwrap();
    assert_eq!(db.list_connections().await.unwrap().len(), 1);

    // Third boot with the URL changed in config.
    let moved = specs_from_block(
        r#"
connections:
  - id: forgejo-main
    kind: forgejo
    base_url: https://git2.example.com
    credential:
      env: FORGEJO_CI_TOKEN
"#,
    );
    provision(db.as_ref(), &moved).await.unwrap();
    let conn = db.get_connection("forgejo-main").await.unwrap().unwrap();
    assert_eq!(conn.base_url, "https://git2.example.com");
    assert_eq!(
        db.config_owned_connection_ids().await.unwrap(),
        vec!["forgejo-main".to_string()]
    );
}

/// Undeclaring hands ownership back instead of deleting: governance hangs off
/// the repo bindings, and the operator gets an editable connection they can
/// remove deliberately.
#[tokio::test]
async fn undeclaring_a_connection_releases_ownership_without_deleting_it() {
    let db = Arc::new(InMemoryDb::new());
    provision(db.as_ref(), &specs_from_block(BLOCK))
        .await
        .unwrap();

    let provisioned = provision(db.as_ref(), &[]).await.unwrap();
    assert_eq!(provisioned.released, vec!["forgejo-main".to_string()]);
    assert!(db.get_connection("forgejo-main").await.unwrap().is_some());
    // The repo binding — the Project — survives.
    assert_eq!(db.repos_of("forgejo-main").await.unwrap().len(), 1);

    // And the API now offers it for editing again: exactly one owner, the DB.
    let app = app(db.clone(), CredentialOverrides::new(), Some("tok-in-store")).await;
    let token = login(&app).await;
    let body = list(&app, &token).await;
    assert_eq!(body[0]["managed_by_config"], false);
}

/// One path, one precedence: where a connection has BOTH a deployment-supplied
/// credential and a stored one, the deployment wins — the generalization of
/// `SCARAB_GITHUB_APP_PEM` overriding the DB `_forge` credential.
#[tokio::test]
async fn the_deployment_override_wins_over_the_secret_store() {
    let secrets = FakeSecrets::new();
    let conn = ForgeConnection {
        id: "forgejo-main".into(),
        kind: ForgeKind::Forgejo,
        base_url: "https://git.example.com".into(),
        credential_ref: "forgejo-main".into(),
    };
    secrets
        .put(
            &SecretScope::Org {
                org: FORGE_CREDENTIALS_ORG.into(),
            },
            Secret {
                key: "forgejo-main".into(),
                value: b"stale-stored-token".to_vec(),
            },
        )
        .await
        .unwrap();

    let overrides = CredentialOverrides::from_specs(&specs_from_block(BLOCK));
    let material = resolve_connection_credential(&overrides, &secrets, &conn)
        .await
        .unwrap();
    assert_eq!(material, b"tok-from-env");

    // With no override the same call falls through to the store — the second and
    // last step of the path.
    let material = resolve_connection_credential(&CredentialOverrides::new(), &secrets, &conn)
        .await
        .unwrap();
    assert_eq!(material, b"stale-stored-token");
}

/// The kind-wide App PEM (`SCARAB_GITHUB_APP_PEM[_FILE]`) is now just an entry in
/// the same override table — and it applies only in App mode, since in token mode
/// a PEM would be used as a bearer token and fail bafflingly.
#[tokio::test]
async fn the_github_app_pem_is_an_override_in_the_same_table() {
    let github = ForgeConnection {
        id: "gh-1".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: "github-app".into(),
    };
    let forgejo = ForgeConnection {
        id: "forgejo-main".into(),
        kind: ForgeKind::Forgejo,
        base_url: "https://git.example.com".into(),
        credential_ref: "forgejo-main".into(),
    };

    let app_mode = CredentialOverrides::new().with_github_app_pem(Some("PEM".into()), true);
    assert_eq!(app_mode.material_for(&github), Some("PEM"));
    // Kind-wide means GitHub-wide, not everything-wide.
    assert_eq!(app_mode.material_for(&forgejo), None);

    let token_mode = CredentialOverrides::new().with_github_app_pem(Some("PEM".into()), false);
    assert_eq!(token_mode.material_for(&github), None);

    // A per-connection override from config beats the kind-wide PEM.
    let both = CredentialOverrides::new()
        .with_connection("gh-1", "explicit")
        .with_github_app_pem(Some("PEM".into()), true);
    assert_eq!(both.material_for(&github), Some("explicit"));
}
