//! Duplicating an org Environment and its display name over HTTP (git-bug
//! d002a0c), with step-up **enforced**.
//!
//! The Environment store is a *real* Postgres (`fresh_db`; skipped without
//! `SCARAB_TEST_DATABASE_URL`). The secret store is the in-memory
//! `FakeSecrets`, wrapped where a test needs it to fail part-way — the
//! secret backend is the external the duplicate cannot share a transaction
//! with.

mod common;
mod step_up_harness;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::{json, Value};
use tokio::sync::Notify;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{Db, DeployContext, RunId, Timestamp};
use scarab_identity::{Role, Scope};
use scarab_policy::policy::Policy;
use scarab_policy::store::{NewPolicy, PolicyScope, PolicyStore};
use scarab_project::{
    ApprovalGateRules, EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProjectPrivilegeGrant, EnvironmentProtectionRules, ImageGrant, MissingGateMode,
    OrgEnvironment, OrgEnvironmentStore,
};
use scarab_secrets::{Secret, SecretError, SecretProvider, SecretScope};
use scarab_testkit::{FakeSecrets, InMemoryPolicies};
use scarab_webauthn::soft::SoftPasskey;

use step_up_harness::*;

const PROD: &str = "/v1/orgs/acme/environments/prod";

fn env_scope(project: &str, environment: &str) -> SecretScope {
    SecretScope::EnvironmentProject {
        org: "acme".into(),
        project: project.into(),
        environment: environment.into(),
    }
}

/// A secret store that refuses the `fail_at`-th write (1-based) and every
/// one after it, as a backend that goes away part-way would.
struct FailingWrites {
    inner: Arc<FakeSecrets>,
    writes: AtomicUsize,
    fail_at: usize,
}

#[async_trait]
impl SecretProvider for FailingWrites {
    async fn get(&self, scope: &SecretScope, key: &str) -> Result<Secret, SecretError> {
        self.inner.get(scope, key).await
    }
    async fn put(&self, scope: &SecretScope, secret: Secret) -> Result<(), SecretError> {
        if self.writes.fetch_add(1, Ordering::SeqCst) + 1 >= self.fail_at {
            return Err(SecretError::Backend("the secret store went away".into()));
        }
        self.inner.put(scope, secret).await
    }
    async fn list_scoped(&self, scope: &SecretScope) -> Result<Vec<String>, SecretError> {
        self.inner.list_scoped(scope).await
    }
    async fn delete(&self, scope: &SecretScope, key: &str) -> Result<(), SecretError> {
        self.inner.delete(scope, key).await
    }
    async fn environment_secret_projects(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<String>, SecretError> {
        self.inner
            .environment_secret_projects(org, environment)
            .await
    }
}

/// A secret store whose first write waits until the test lets it go: the
/// Duplicate making it has passed its checks and holds its claim on the new
/// name, and has written nothing yet. The test hook that puts a second
/// request between one Duplicate's checks and its Environment's
/// transaction.
struct PausingWrites {
    inner: Arc<FakeSecrets>,
    writes: AtomicUsize,
    /// Notified when the first write arrives.
    paused: Arc<Notify>,
    /// Lets the first write go on.
    resume: Arc<Notify>,
}

#[async_trait]
impl SecretProvider for PausingWrites {
    async fn get(&self, scope: &SecretScope, key: &str) -> Result<Secret, SecretError> {
        self.inner.get(scope, key).await
    }
    async fn put(&self, scope: &SecretScope, secret: Secret) -> Result<(), SecretError> {
        if self.writes.fetch_add(1, Ordering::SeqCst) == 0 {
            self.paused.notify_one();
            self.resume.notified().await;
        }
        self.inner.put(scope, secret).await
    }
    async fn list_scoped(&self, scope: &SecretScope) -> Result<Vec<String>, SecretError> {
        self.inner.list_scoped(scope).await
    }
    async fn delete(&self, scope: &SecretScope, key: &str) -> Result<(), SecretError> {
        self.inner.delete(scope, key).await
    }
    async fn environment_secret_projects(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<String>, SecretError> {
        self.inner
            .environment_secret_projects(org, environment)
            .await
    }
}

/// A fixture whose secret store is [`PausingWrites`], with its two
/// signals.
async fn pausing_fixture() -> Option<(Fixture, Arc<Notify>, Arc<Notify>)> {
    let paused = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let (p, r) = (paused.clone(), resume.clone());
    let f = fixture(move |inner| {
        Arc::new(PausingWrites {
            inner,
            writes: AtomicUsize::new(0),
            paused: p,
            resume: r,
        })
    })
    .await?;
    Some((f, paused, resume))
}

struct Fixture {
    h: Harness,
    pg: Arc<PostgresDb>,
    secrets: Arc<FakeSecrets>,
    tdb: common::TestDb,
}

/// `prod` in `acme`, governed (strict baseline, two eligible patterns, an
/// overlay for `web`, a grant for `api`), with secrets in two Projects'
/// EnvironmentProjectSecretScopes, a Policy naming it, and a Run history.
/// `wrap` decides the secret store the app writes through.
async fn fixture(
    wrap: impl FnOnce(Arc<FakeSecrets>) -> Arc<dyn SecretProvider>,
) -> Option<Fixture> {
    let tdb = fresh_db().await?;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    pg.put_org_environment(&OrgEnvironment {
        org: "acme".into(),
        name: "prod".into(),
        display_name: Some("Production".into()),
        description: "the one prod".into(),
        protection: Default::default(),
        eligibility: Default::default(),
        created_at: 1,
        updated_at: 1,
        updated_by: "seed".into(),
    })
    .await
    .unwrap();
    pg.put_environment_protection(
        "acme",
        "prod",
        &EnvironmentProtectionRules {
            approvers: vec!["amy".into()],
            allowed_refs: vec!["refs/heads/main".into()],
            wait_timer_s: 30,
            require_reason: true,
            approval_gate: ApprovalGateRules {
                missing_gate: MissingGateMode::Refuse,
                added_gate_expiry_s: Some(3600),
                required_approvals: Some(1),
                max_approval_age_s: 600,
            },
        },
        "seed",
        2,
    )
    .await
    .unwrap();
    pg.put_environment_eligibility(
        "acme",
        "prod",
        &EnvironmentProjectEligibility::Projects(vec!["api".into(), "web*".into()]),
        "seed",
        3,
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
        4,
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
            permit_k8s_overlay: true,
        },
        "seed",
        5,
    )
    .await
    .unwrap();

    let secrets = Arc::new(
        FakeSecrets::new()
            .with_secret(&env_scope("web", "prod"), "DB_PASSWORD", b"web-pw")
            .with_secret(&env_scope("web", "prod"), "API_KEY", b"web-key")
            .with_secret(&env_scope("api", "prod"), "DB_PASSWORD", b"api-pw")
            // Another Environment's and a Project-wide secret are not copied.
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
    let policies = Arc::new(InMemoryPolicies::new());
    for (id, scope, environments) in [
        (
            "p-prod",
            PolicyScope::Org { org: "acme".into() },
            json!(["prod"]),
        ),
        (
            "p-web",
            PolicyScope::Project {
                org: "acme".into(),
                project: "web".into(),
            },
            json!(["staging", "prod"]),
        ),
        // `prod*` selects the copy too: nothing to change.
        (
            "p-glob",
            PolicyScope::Org { org: "acme".into() },
            json!(["prod*"]),
        ),
        // Every Environment: nothing to change.
        ("p-all", PolicyScope::Org { org: "acme".into() }, json!([])),
    ] {
        policies
            .create(&NewPolicy {
                id: id.into(),
                scope,
                body: Policy::from_json(json!({
                    "name": id,
                    "checkpoint": "run.create",
                    "match": { "environments": environments },
                    "rules": [{ "when": "true", "effect": "deny", "message": "no" }]
                }))
                .unwrap(),
                author: "seed".into(),
                reason: "seed".into(),
                at: 0,
            })
            .await
            .unwrap();
    }

    let app_secrets = wrap(secrets.clone());
    let pg_envs = pg.clone();
    let h = harness_with(move |st| {
        st.with_org_environments(pg_envs.clone())
            .with_environment_governance(pg_envs)
            .with_secrets(app_secrets)
            .with_policies(policies)
    });
    for (run, project, environment) in [
        ("r1", "web", "prod"),
        ("r2", "checkout", "prod"),
        ("r3", "web", "prod"),
        ("r4", "jobs", "staging"),
    ] {
        let run = RunId(run.into());
        h.db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
        h.db.set_run_deploy_context(
            &run,
            &DeployContext {
                org: "acme".into(),
                project: project.into(),
                environment: environment.into(),
                git_ref: "refs/heads/main".into(),
                locked_out: false,
            },
        )
        .await
        .unwrap();
    }
    Some(Fixture {
        h,
        pg,
        secrets,
        tdb,
    })
}

async fn signed_in_admin(h: &Harness) -> (Sess, SoftPasskey) {
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (s, soft)
}

/// Everything the Environment store and the secret store hold for `env`.
async fn snapshot(
    f: &Fixture,
    env: &str,
) -> (Option<OrgEnvironment>, Value, Vec<(String, Vec<u8>)>) {
    let environment = f.pg.get_org_environment("acme", env).await.unwrap();
    let projects = match f.pg.environment_projects("acme", env).await {
        Ok(records) => json!(format!("{records:?}")),
        Err(e) => json!(e.to_string()),
    };
    let mut secrets = Vec::new();
    for project in f
        .secrets
        .environment_secret_projects("acme", env)
        .await
        .unwrap()
    {
        let scope = env_scope(&project, env);
        let mut keys = f.secrets.list_scoped(&scope).await.unwrap();
        keys.sort();
        for key in keys {
            let value = f.secrets.get(&scope, &key).await.unwrap().value;
            secrets.push((format!("{project}/{key}"), value));
        }
    }
    (environment, projects, secrets)
}

#[tokio::test]
async fn a_duplicate_copies_rules_eligibility_projects_and_secret_values() {
    let Some(f) = fixture(|s| s).await else {
        return;
    };
    let h = &f.h;
    let (s, mut soft) = signed_in_admin(h).await;
    let before = snapshot(&f, "prod").await;
    let uri = format!("{PROD}/duplicate");

    // A step-up: no open window, no copy.
    let (st, b) = h
        .cookie("POST", &uri, Some(json!({ "name": "prod-eu" })), &s)
        .await;
    assert_step_up(st, &b, "window");
    assert_eq!(b["op"], "environment.duplicate", "{b}");
    assert_eq!(
        snapshot(&f, "prod-eu").await,
        (
            None,
            json!("org `acme` has no environment `prod-eu`"),
            vec![]
        )
    );

    h.open_window(&s, &mut soft).await;
    let (st, b) = h
        .cookie("POST", &uri, Some(json!({ "name": "prod-eu" })), &s)
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");

    // The new Environment: the old one's rules and eligibility, the display
    // name it shows plus " (copy)".
    let env = &b["environment"];
    assert_eq!(env["name"], "prod-eu");
    assert_eq!(env["display_name"], "Production (copy)");
    assert_eq!(env["description"], "the one prod");
    assert_eq!(
        env["protection"],
        json!({
            "approvers": ["amy"],
            "allowed_refs": ["refs/heads/main"],
            "wait_timer_s": 30,
            "require_reason": true,
            "approval_gate": {
                "missing_gate_mode": "refuse",
                "added_gate_expiry_s": 3600,
                "required_approvals": 1,
                "max_approval_age_s": 600
            }
        })
    );
    assert_eq!(
        env["eligibility"],
        json!({ "all_projects": false, "projects": ["api", "web*"] })
    );
    assert_eq!(env["updated_by"], "amy");

    // Overlays and grants, re-keyed; secret values in the new scopes.
    let copy = snapshot(&f, "prod-eu").await;
    assert_eq!(
        copy.1,
        json!(format!(
            "{:?}",
            f.pg.environment_projects("acme", "prod")
                .await
                .unwrap()
                .into_iter()
                .map(|mut r| {
                    r.overlay
                        .iter_mut()
                        .for_each(|o| o.environment = "prod-eu".into());
                    r.privilege_grant
                        .iter_mut()
                        .for_each(|g| g.environment = "prod-eu".into());
                    r
                })
                .collect::<Vec<_>>()
        ))
    );
    assert_eq!(
        copy.2,
        vec![
            ("api/DB_PASSWORD".to_string(), b"api-pw".to_vec()),
            ("web/API_KEY".to_string(), b"web-key".to_vec()),
            ("web/DB_PASSWORD".to_string(), b"web-pw".to_vec()),
        ]
    );
    assert_eq!(
        b["copied"],
        json!({
            "overlays": ["web"],
            "privilege_grants": ["api"],
            "secrets": [
                { "project": "api", "keys": ["DB_PASSWORD"] },
                { "project": "web", "keys": ["API_KEY", "DB_PASSWORD"] }
            ]
        })
    );
    assert!(
        !b.to_string().contains("web-pw"),
        "the answer never carries a value: {b}"
    );

    // The checklist of what was not copied.
    let not = &b["not_copied"];
    assert_eq!(
        not["policies"],
        json!([
            { "id": "p-prod", "name": "p-prod", "project": null, "environments": ["prod"] },
            { "id": "p-web", "name": "p-web", "project": "web", "environments": ["staging", "prod"] }
        ])
    );
    assert_eq!(not["projects_with_runs"], json!(["checkout", "web"]));
    assert_eq!(
        not["old_oidc_subject"],
        "scarab:org/acme/repo/*/env/prod/ref/*"
    );
    assert_eq!(
        not["new_oidc_subject"],
        "scarab:org/acme/repo/*/env/prod-eu/ref/*"
    );

    // The old Environment is not changed.
    assert_eq!(snapshot(&f, "prod").await, before);

    // A display name given is used as is.
    let (st, b) = h
        .cookie(
            "POST",
            &uri,
            Some(json!({ "name": "prod-us", "display_name": " Production (US) " })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert_eq!(b["environment"]["display_name"], "Production (US)");

    f.tdb.cleanup().await;
}

#[tokio::test]
async fn a_taken_name_is_409_and_changes_nothing() {
    let Some(f) = fixture(|s| s).await else {
        return;
    };
    let h = &f.h;
    let (s, mut soft) = signed_in_admin(h).await;
    h.open_window(&s, &mut soft).await;
    let uri = format!("{PROD}/duplicate");

    let (st, _) = h
        .cookie(
            "PUT",
            "/v1/orgs/acme/environments/staging",
            Some(json!({})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let staging = snapshot(&f, "staging").await;
    let prod = snapshot(&f, "prod").await;

    for name in ["staging", "prod"] {
        let (st, b) = h
            .cookie("POST", &uri, Some(json!({ "name": name })), &s)
            .await;
        assert_eq!(st, StatusCode::CONFLICT, "{name}: {b}");
    }
    assert_eq!(snapshot(&f, "staging").await, staging);
    assert_eq!(snapshot(&f, "prod").await, prod);

    // Secrets left under a name no Environment has are refused too, so a
    // copy is never mixed with values it did not carry.
    f.secrets
        .put(
            &env_scope("web", "prod-eu"),
            Secret {
                key: "OLD".into(),
                value: b"stale".to_vec(),
            },
        )
        .await
        .unwrap();
    let (st, b) = h
        .cookie("POST", &uri, Some(json!({ "name": "prod-eu" })), &s)
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert_eq!(
        f.pg.get_org_environment("acme", "prod-eu").await.unwrap(),
        None
    );

    // Unknown source, bad names.
    let (st, _) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/environments/nope/duplicate",
            Some(json!({ "name": "x" })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    for body in [
        json!({ "name": ".." }),
        json!({ "name": "x", "display_name": "bad\u{7}" }),
        json!({ "name": "x", "secrets": true }),
    ] {
        let (st, b) = h.cookie("POST", &uri, Some(body.clone()), &s).await;
        assert!(
            st.is_client_error() && st != StatusCode::CONFLICT,
            "{body}: {st} {b}"
        );
    }
    assert_eq!(f.pg.get_org_environment("acme", "x").await.unwrap(), None);

    f.tdb.cleanup().await;
}

/// The secret store fails on the second copied secret: the first is removed
/// again and no Environment is created.
#[tokio::test]
async fn a_failure_part_way_leaves_no_copy() {
    let Some(f) = fixture(|inner| {
        Arc::new(FailingWrites {
            inner,
            writes: AtomicUsize::new(0),
            fail_at: 2,
        })
    })
    .await
    else {
        return;
    };
    let h = &f.h;
    let (s, mut soft) = signed_in_admin(h).await;
    h.open_window(&s, &mut soft).await;
    let prod = snapshot(&f, "prod").await;

    let (st, b) = h
        .cookie(
            "POST",
            &format!("{PROD}/duplicate"),
            Some(json!({ "name": "prod-eu" })),
            &s,
        )
        .await;
    assert!(st.is_server_error(), "{st} {b}");
    let (environment, _, secrets) = snapshot(&f, "prod-eu").await;
    assert_eq!(environment, None, "no Environment");
    assert_eq!(secrets, vec![], "no secret left behind");
    assert!(f.pg.environment_projects("acme", "prod-eu").await.is_err());
    assert_eq!(snapshot(&f, "prod").await, prod);

    f.tdb.cleanup().await;
}

/// Two Duplicates to one new name, the second sent while the first sits
/// between its checks and its first secret write: the second is refused
/// before it writes anything, and the first's Environment keeps every
/// secret it copied. Without the claim the second wins, and the first's
/// cleanup deletes the secrets the second's Environment holds.
#[tokio::test]
async fn a_second_duplicate_to_the_same_name_is_409_and_takes_no_secret() {
    let Some((f, paused, resume)) = pausing_fixture().await else {
        return;
    };
    let h = &f.h;
    let (s, mut soft) = signed_in_admin(h).await;
    h.open_window(&s, &mut soft).await;
    let uri = format!("{PROD}/duplicate");

    let ((first, first_body), (second, second_body)) = tokio::join!(
        h.cookie("POST", &uri, Some(json!({ "name": "prod-eu" })), &s),
        async {
            paused.notified().await;
            let answer = h
                .cookie(
                    "POST",
                    &uri,
                    Some(json!({ "name": "prod-eu", "display_name": "Second" })),
                    &s,
                )
                .await;
            resume.notify_one();
            answer
        }
    );
    assert_eq!(first, StatusCode::CREATED, "{first_body}");
    assert_eq!(second, StatusCode::CONFLICT, "{second_body}");

    let (environment, _, secrets) = snapshot(&f, "prod-eu").await;
    assert_eq!(
        environment.and_then(|e| e.display_name).as_deref(),
        Some("Production (copy)"),
        "the first Duplicate's Environment"
    );
    assert_eq!(
        secrets,
        vec![
            ("api/DB_PASSWORD".to_string(), b"api-pw".to_vec()),
            ("web/API_KEY".to_string(), b"web-key".to_vec()),
            ("web/DB_PASSWORD".to_string(), b"web-pw".to_vec()),
        ],
        "every secret the answer lists is there"
    );

    // The claim is released: a third Duplicate to another name goes ahead.
    let (st, b) = h
        .cookie("POST", &uri, Some(json!({ "name": "prod-us" })), &s)
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");

    f.tdb.cleanup().await;
}

/// The Environment store refuses the copy after the secrets are written (a
/// plain create took the name in between): the copied secrets are removed
/// and the Environment that took the name is left as it was.
#[tokio::test]
async fn a_name_taken_after_the_secrets_are_copied_removes_them() {
    let Some((f, paused, resume)) = pausing_fixture().await else {
        return;
    };
    let h = &f.h;
    let (s, mut soft) = signed_in_admin(h).await;
    h.open_window(&s, &mut soft).await;
    let uri = format!("{PROD}/duplicate");

    let ((st, b), taken) = tokio::join!(
        h.cookie("POST", &uri, Some(json!({ "name": "prod-eu" })), &s),
        async {
            paused.notified().await;
            let taken = h
                .cookie(
                    "PUT",
                    "/v1/orgs/acme/environments/prod-eu",
                    Some(json!({ "description": "made by hand" })),
                    &s,
                )
                .await;
            resume.notify_one();
            taken
        }
    );
    assert_eq!(taken.0, StatusCode::OK, "{}", taken.1);
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    let (environment, _, secrets) = snapshot(&f, "prod-eu").await;
    assert_eq!(
        environment.map(|e| e.description).as_deref(),
        Some("made by hand")
    );
    assert_eq!(secrets, vec![], "the copied secrets are removed");

    f.tdb.cleanup().await;
}

/// An admin of another org may not duplicate this org's Environment.
#[tokio::test]
async fn an_admin_of_another_org_is_403() {
    let Some(f) = fixture(|s| s).await else {
        return;
    };
    let h = &f.h;
    h.grant("zed", Scope::Org("other".into()), Role::Owner)
        .await;
    let s = h.browser_session(scoped("zed")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    h.open_window(&s, &mut soft).await;

    let (st, b) = h
        .cookie(
            "POST",
            &format!("{PROD}/duplicate"),
            Some(json!({ "name": "prod-eu" })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(snapshot(&f, "prod-eu").await.2, vec![]);
    assert_eq!(
        f.pg.get_org_environment("acme", "prod-eu").await.unwrap(),
        None
    );

    f.tdb.cleanup().await;
}

/// The display name is written by the whole-Environment PUT with no
/// step-up (it is not a loosening), shown by every read, and cleared by a
/// PUT without it.
#[tokio::test]
async fn the_display_name_is_edited_without_a_step_up() {
    let Some(f) = fixture(|s| s).await else {
        return;
    };
    let h = &f.h;
    let (s, _soft) = signed_in_admin(h).await;

    let (st, b) = h
        .cookie(
            "PUT",
            PROD,
            Some(json!({ "display_name": "Production (EU)", "description": "d" })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["name"], "prod");
    assert_eq!(b["display_name"], "Production (EU)");
    let (_, list) = h
        .cookie("GET", "/v1/orgs/acme/environments", None, &s)
        .await;
    assert_eq!(list[0]["display_name"], "Production (EU)");
    let (_, project_view) = h
        .cookie("GET", "/v1/repos/acme/web/environments", None, &s)
        .await;
    assert_eq!(
        project_view[0]["display_name"], "Production (EU)",
        "{project_view}"
    );

    // Blank reads as none.
    let (st, b) = h
        .cookie("PUT", PROD, Some(json!({ "display_name": "  " })), &s)
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["display_name"], Value::Null);

    f.tdb.cleanup().await;
}
