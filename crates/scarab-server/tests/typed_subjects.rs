//! Admin-typed subjects must name a Principal (ADR-0069 §3), through the real
//! router against *real* Postgres. People type handles; what is stored is the
//! Principal's id, and what is read back is the handle (ADR-0080).
//!
//! Before: `put_binding` checked only for an empty string and approver lists
//! were free text matched by exact equality, so a grant typed for someone who
//! had not signed in yet was claimed by whoever FIRST produced that string at
//! sign-in — the recycled-login takeover re-opened for every pre-assigned
//! surface (found by the ADR-0069 audit). Now a grant and an approver entry are
//! bound to a pinned Principal at the moment they are written.
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::Db;
use scarab_identity::{IdentityStore, ProviderIdentity, RbacStore, Role, Scope};
use scarab_project::{EnvironmentGovernanceStore, OrgEnvironmentStore};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryObjectStore};

fn app(pg: Arc<PostgresDb>) -> axum::Router {
    let db: Arc<dyn Db> = pg.clone();
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    // Authn is off (every caller is the synthetic Owner): this suite is about
    // what a write may NAME, not who may write.
    router(
        AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
            .with_org_environments(pg.clone())
            .with_environment_governance(pg.clone())
            .with_rbac(pg.clone())
            .with_identities(pg.clone()),
    )
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn github(external_id: &str, handle: &str) -> ProviderIdentity {
    ProviderIdentity {
        provider: "github".into(),
        external_id: external_id.into(),
        handle: handle.into(),
        email: None,
        email_verified: false,
        display_name: Some("Alice Liddell".into()),
    }
}

#[tokio::test]
async fn grants_and_approvers_must_name_an_existing_principal() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = app(pg.clone());

    let alice = pg
        .sign_in(
            &github("1001", "alice"),
            "github:alice",
            scarab_identity::Placeholder::Never,
            1,
        )
        .await
        .unwrap();
    assert_eq!(alice.handle, "github:alice");
    let alice = alice.id;
    assert!(scarab_identity::is_principal_id(&alice), "{alice}");

    // A grant to someone who has never signed in is refused, and nothing is
    // written — the string stays unclaimable.
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/bindings",
        Some(serde_json::json!({ "subject": "github:bob", "role": "member" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{msg}");
    assert!(msg.contains("not a Scarab user"), "{msg}");
    assert_eq!(
        pg.role_of("github:bob", &Scope::Org("acme".into()))
            .await
            .unwrap(),
        None
    );

    // A grant typed as the handle lands under the Principal's id, and the
    // handle typed in another case names the same one.
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/bindings",
        Some(serde_json::json!({ "subject": "GitHub:Alice", "role": "member" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{msg}");
    assert_eq!(
        pg.role_of(&alice, &Scope::Org("acme".into()))
            .await
            .unwrap(),
        Some(Role::Member)
    );
    assert_eq!(
        pg.role_of("github:alice", &Scope::Org("acme".into()))
            .await
            .unwrap(),
        None,
        "the handle is never a key"
    );
    // The listing names the person by id and handle.
    let (status, body) = send(&app, "GET", "/v1/orgs/acme/bindings", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(listed[0]["id"], alice.as_str());
    assert_eq!(listed[0]["handle"], "github:alice");
    assert_eq!(listed[0]["display_name"], "Alice Liddell");
    // One member, by id: the member page's read.
    let (status, body) = send(&app, "GET", &format!("/v1/orgs/acme/members/{alice}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let member: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(member["handle"], "github:alice");
    assert_eq!(member["claimed"], true);
    assert_eq!(member["bindings"][0]["role"], "member");
    let (status, _) = send(
        &app,
        "GET",
        "/v1/orgs/acme/members/p_00000000000000000000000000000000",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A merged id follows to its target (the link/merge flow writes these;
    // staged directly here because that flow is a later slice).
    sqlx::query(
        "INSERT INTO principals (handle, created_at, merged_into) VALUES ('okta:alice', 1, $1)",
    )
    .bind(&alice)
    .execute(&tdb.pool)
    .await
    .unwrap();
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/bindings",
        Some(serde_json::json!({ "subject": "okta:alice", "role": "admin", "project": "web" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{msg}");
    let web = Scope::Project {
        org: "acme".into(),
        name: "web".into(),
    };
    assert_eq!(pg.role_of(&alice, &web).await.unwrap(), Some(Role::Admin));
    let folded = pg.by_handle("okta:alice").await.unwrap()[0].id.clone();
    assert_eq!(pg.role_of(&folded, &web).await.unwrap(), None);

    // Approver lists. The per-repo write that took them is retired (410,
    // ADR-0071); approvers are written on an org Environment's baseline and
    // on a Project's overlay, and both check them exactly as it did.
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/repos/acme/web/environments/prod",
        Some(serde_json::json!({
            "approvers": ["github:mallory"], "wait_timer": 0, "allowed_refs": [], "concurrency": 1,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{msg}");
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/prod",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/prod/eligibility",
        Some(serde_json::json!({ "all_projects": false, "projects": ["web"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    let approvers = |approvers: &[&str]| serde_json::json!({ "approvers": approvers });
    let overlay = "/v1/orgs/acme/environments/prod/projects/web/overlay";
    let baseline = "/v1/orgs/acme/environments/prod/protection";

    // …one unknown entry refuses the whole write, on either layer…
    for uri in [overlay, baseline] {
        let (status, msg) = send(
            &app,
            "PUT",
            uri,
            Some(approvers(&["github:alice", "github:mallory"])),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {msg}");
        assert!(msg.contains("github:mallory"), "{uri}: {msg}");
    }
    let stored = pg
        .deploy_governance("acme", "web", "prod")
        .await
        .unwrap()
        .unwrap();
    assert!(stored.overlay.is_none(), "nothing written");
    assert!(
        stored.environment.protection.approvers.is_empty(),
        "nothing written"
    );

    // …and a merged entry is stored as the Principal it now is.
    for uri in [overlay, baseline] {
        let (status, msg) = send(&app, "PUT", uri, Some(approvers(&["okta:alice"]))).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {msg}");
    }
    let stored = pg
        .deploy_governance("acme", "web", "prod")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.overlay.unwrap().rules.approvers, vec![alice.clone()]);
    assert_eq!(
        pg.get_org_environment("acme", "prod")
            .await
            .unwrap()
            .unwrap()
            .protection
            .approvers,
        vec![alice.clone()]
    );
    // Read back, the approver is the handle people typed.
    let (status, body) = send(&app, "GET", baseline, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let read: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(read["approvers"], serde_json::json!(["github:alice"]));

    // The typeahead: by handle prefix, by display-name fragment, never merged
    // ids, and LIKE wildcards are literal.
    let (status, body) = send(&app, "GET", "/v1/orgs/acme/principals?q=ali", None).await;
    assert_eq!(status, StatusCode::OK);
    let found: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        found,
        serde_json::json!([{
            "id": alice, "handle": "github:alice", "display_name": "Alice Liddell",
            "claimed": true,
            "identities": [{ "provider": "github", "handle": "alice" }],
        }])
    );
    let (_, body) = send(&app, "GET", "/v1/orgs/acme/principals?q=Liddell", None).await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()[0]["handle"],
        "github:alice"
    );
    let (_, body) = send(&app, "GET", "/v1/orgs/acme/principals?q=%25", None).await;
    assert_eq!(body, "[]", "`%` is matched literally, not as a wildcard");

    tdb.cleanup().await;
}
