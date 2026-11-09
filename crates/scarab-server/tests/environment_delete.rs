//! Deleting an org Environment over HTTP, with step-up **enforced**
//! (git-bug a144fb1): a plain delete is refused while Projects hold
//! overlays, privilege grants or secrets in it, and the refusal lists them
//! with a count per kind; `?cascade=true` deletes them with it. Only an org
//! admin may delete, after a step-up, and the security log records what
//! went.
//!
//! The Environment store is a *real* Postgres (`fresh_db`; skipped without
//! `SCARAB_TEST_DATABASE_URL`). The secret store is the in-memory
//! `FakeSecrets`.

mod common;
mod step_up_harness;

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{Role, Scope};
use scarab_project::{
    EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProjectPrivilegeGrant, EnvironmentProtectionRules, ImageGrant, OrgEnvironment,
    OrgEnvironmentStore,
};
use scarab_secrets::{SecretProvider, SecretScope};
use scarab_testkit::FakeSecrets;

use step_up_harness::*;

const PROD: &str = "/v1/orgs/acme/environments/prod";

fn env_scope(project: &str, environment: &str) -> SecretScope {
    SecretScope::EnvironmentProject {
        org: "acme".into(),
        project: project.into(),
        environment: environment.into(),
    }
}

struct Fixture {
    h: Harness,
    pg: Arc<PostgresDb>,
    secrets: Arc<FakeSecrets>,
    tdb: common::TestDb,
}

/// `prod` in `acme` admitting `web*` and `api`, with an overlay for `web`,
/// a grant for `api`, two secrets of `web` and one of `api` in it; and a
/// `staging` secret and a Project-wide secret a delete of `prod` must keep.
async fn fixture() -> Option<Fixture> {
    let tdb = fresh_db().await?;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    pg.put_org_environment(&OrgEnvironment {
        org: "acme".into(),
        name: "prod".into(),
        display_name: None,
        description: String::new(),
        protection: Default::default(),
        eligibility: Default::default(),
        created_at: 1,
        updated_at: 1,
        updated_by: "seed".into(),
    })
    .await
    .unwrap();
    pg.put_environment_eligibility(
        "acme",
        "prod",
        &EnvironmentProjectEligibility::Projects(vec!["api".into(), "web*".into()]),
        "seed",
        2,
    )
    .await
    .unwrap();
    pg.put_environment_project_overlay(
        &EnvironmentProjectOverlay {
            org: "acme".into(),
            environment: "prod".into(),
            project: "web".into(),
            rules: EnvironmentProtectionRules {
                approvers: vec!["bob".into()],
                ..Default::default()
            },
        },
        "seed",
        3,
    )
    .await
    .unwrap();
    pg.put_environment_project_privilege_grant(
        &EnvironmentProjectPrivilegeGrant {
            org: "acme".into(),
            environment: "prod".into(),
            project: "api".into(),
            privileged_images: vec![ImageGrant {
                image_digest: format!("sha256:{}", "a".repeat(64)),
                privileged: true,
                capabilities: vec![],
            }],
            permit_k8s_overlay: false,
        },
        "seed",
        4,
    )
    .await
    .unwrap();
    let secrets = Arc::new(
        FakeSecrets::new()
            .with_secret(&env_scope("web", "prod"), "DB_PASSWORD", b"web-pw")
            .with_secret(&env_scope("web", "prod"), "API_KEY", b"web-key")
            .with_secret(&env_scope("api", "prod"), "DB_PASSWORD", b"api-pw")
            .with_secret(&env_scope("web", "staging"), "DB_PASSWORD", b"stg")
            .with_secret(
                &SecretScope::Repo {
                    org: "acme".into(),
                    repo: "web".into(),
                },
                "SHARED",
                b"shared",
            ),
    );
    let (pg_envs, app_secrets) = (pg.clone(), secrets.clone());
    let h = harness_with(move |st| {
        st.with_org_environments(pg_envs.clone())
            .with_environment_governance(pg_envs)
            .with_secrets(app_secrets)
    });
    Some(Fixture {
        h,
        pg,
        secrets,
        tdb,
    })
}

/// Every secret key of `env`, as `project/key`, sorted.
async fn secret_keys(f: &Fixture, env: &str) -> Vec<String> {
    let mut out = Vec::new();
    for project in f
        .secrets
        .environment_secret_projects("acme", env)
        .await
        .unwrap()
    {
        for key in f
            .secrets
            .list_scoped(&env_scope(&project, env))
            .await
            .unwrap()
        {
            out.push(format!("{project}/{key}"));
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn a_plain_delete_lists_what_blocks_it_and_a_cascade_deletes_it_all() {
    let Some(f) = fixture().await else {
        return;
    };
    let h = &f.h;
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    let cascade = format!("{PROD}?cascade=true");

    // A step-up first, as for any secret delete: no window, nothing goes.
    let (st, b) = h.cookie("DELETE", &cascade, None, &s).await;
    assert_step_up(st, &b, "window");
    assert_eq!(b["op"], "environment.delete", "{b}");
    assert!(f
        .pg
        .get_org_environment("acme", "prod")
        .await
        .unwrap()
        .is_some());
    h.open_window(&s, &mut soft).await;

    // Without `cascade`: 409, and the body names every holder by kind.
    let (st, b) = h.cookie("DELETE", PROD, None, &s).await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["code"], "environment_in_use", "{b}");
    assert_eq!(b["overlay_count"], 1, "{b}");
    assert_eq!(b["privilege_grant_count"], 1, "{b}");
    assert_eq!(b["secret_count"], 3, "{b}");
    assert_eq!(b["overlays"], json!(["web"]));
    assert_eq!(b["privilege_grants"], json!(["api"]));
    assert_eq!(
        b["secrets"],
        json!([
            { "project": "api", "keys": ["DB_PASSWORD"] },
            { "project": "web", "keys": ["API_KEY", "DB_PASSWORD"] },
        ])
    );
    assert!(f
        .pg
        .get_org_environment("acme", "prod")
        .await
        .unwrap()
        .is_some());
    assert_eq!(secret_keys(&f, "prod").await.len(), 3, "nothing deleted");

    // With it: the Environment, its overlay, grant and secrets go.
    let (st, b) = h.cookie("DELETE", &cascade, None, &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");
    assert!(f
        .pg
        .get_org_environment("acme", "prod")
        .await
        .unwrap()
        .is_none());
    assert_eq!(secret_keys(&f, "prod").await, Vec::<String>::new());
    // Created again, the name starts with nothing of the old one.
    let (st, b) = h.cookie("PUT", PROD, Some(json!({})), &s).await;
    assert!(st.is_success(), "{st} {b}");
    assert_eq!(
        f.pg.environment_projects("acme", "prod").await.unwrap(),
        vec![]
    );
    // Another Environment's secret and a Project-wide one stay.
    assert_eq!(secret_keys(&f, "staging").await, vec!["web/DB_PASSWORD"]);
    assert!(f
        .secrets
        .get(
            &SecretScope::Repo {
                org: "acme".into(),
                repo: "web".into()
            },
            "SHARED"
        )
        .await
        .is_ok());

    // The security log records what went, by name.
    let completed = h.events("op.completed").await;
    let deleted = completed
        .iter()
        .find(|e| e.op.as_deref() == Some("environment.delete") && e.detail["status"] == 204)
        .unwrap_or_else(|| panic!("no completed delete: {completed:?}"));
    assert_eq!(
        deleted.detail["deleted"],
        json!({
            "cascade": true,
            "overlay_count": 1,
            "privilege_grant_count": 1,
            "secret_count": 3,
            "overlays": ["web"],
            "privilege_grants": ["api"],
            "secrets": [
                { "project": "api", "keys": ["DB_PASSWORD"] },
                { "project": "web", "keys": ["API_KEY", "DB_PASSWORD"] },
            ],
        })
    );
    f.tdb.cleanup().await;
}

/// The admin of a Project holding an overlay cannot delete the Environment,
/// with or without `cascade`, even after a step-up: deleting stays an org
/// admin's. An org member is refused before any step-up is asked.
#[tokio::test]
async fn only_an_org_admin_deletes_with_cascade() {
    let Some(f) = fixture().await else {
        return;
    };
    let h = &f.h;
    h.grant(
        "pat",
        Scope::Project {
            org: "acme".into(),
            name: "web".into(),
        },
        Role::Admin,
    )
    .await;
    h.grant("mo", Scope::Org("acme".into()), Role::Member).await;
    for who in ["pat", "mo"] {
        let s = h.browser_session(scoped(who)).await;
        if who == "pat" {
            // An admin somewhere is asked for a step-up first.
            let mut soft = passkey();
            h.enroll(&s, &mut soft, "Laptop").await;
            h.open_window(&s, &mut soft).await;
        }
        for uri in [PROD.to_string(), format!("{PROD}?cascade=true")] {
            let (st, b) = h.cookie("DELETE", &uri, None, &s).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{who} {uri}: {b}");
            assert_eq!(b, json!("insufficient role"), "{who} {uri}");
        }
    }
    assert!(f
        .pg
        .get_org_environment("acme", "prod")
        .await
        .unwrap()
        .is_some());
    assert_eq!(secret_keys(&f, "prod").await.len(), 3);
    f.tdb.cleanup().await;
}
