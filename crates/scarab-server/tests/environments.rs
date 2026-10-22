//! Environments + protection rules acceptance (ADR-0024, 0037) against *real*
//! Postgres via the HTTP surface. Two things are covered:
//!
//!  1. Environment **management**: since ADR-0071 the per-repo writes are
//!     410 and management is the org Environment, its baseline and
//!     eligibility, and the Project's overlay and privilege grant; the
//!     repo-scoped reads (one / list / legacy history) stay. Plus the
//!     migration's split report.
//!  2. The new **admission model**: a deploy run suspends at a manual gate, and
//!     approving it releases the run and records a deployment only once the
//!     accumulated approvers satisfy the environment's rules (ADR-0037) — not by
//!     a caller-supplied approvals list (the old `POST …/deploy` is retired).
//!
//! Test-mode auth is disabled, so every approver is the `anonymous` principal;
//! we drive `admits` with that subject. Multi-distinct-approver accumulation is
//! covered at the engine level (`scarab-db-postgres/tests/gate.rs`).
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::fresh_db;
use common::governance::seed_migrated_environment;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    Db, DeployContext, RunId, RunStatus, Scheduler, StepId, StepSpec, StepStatus, Timestamp,
};
use scarab_project::{
    EnvironmentGovernanceStore, EnvironmentStore, OrgEnvironmentStore, ProtectionRules,
};
use scarab_secrets::{SecretProvider, SecretScope};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, FakeSecrets, InMemoryObjectStore};

fn rules(approvers: &[&str], allowed_refs: &[&str]) -> ProtectionRules {
    ProtectionRules {
        approvers: approvers.iter().map(|s| s.to_string()).collect(),
        wait_timer: 0,
        allowed_refs: allowed_refs.iter().map(|s| s.to_string()).collect(),
        concurrency: 1,
        secret_scope: SecretScope::EnvironmentProject {
            org: "acme".into(),
            project: "web".into(),
            environment: "prod".into(),
        },
        oidc_subject: "scarab:acme/web/prod".into(),
        privileged_images: Vec::new(),
        permit_k8s_overlay: false,
        require_reason: false,
    }
}

fn app_state(pg: Arc<PostgresDb>) -> AppState {
    let db: Arc<dyn Db> = pg.clone();
    let envs: Arc<dyn EnvironmentStore> = pg.clone();
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    AppState::new(db, clock, logs)
        .with_environments(envs)
        .with_org_environments(pg.clone())
        .with_environment_governance(pg)
}

fn app(pg: Arc<PostgresDb>) -> axum::Router {
    router(app_state(pg))
}

fn json_req(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn get_req(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// One request through the router: the status and the body as text.
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

fn json_of(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

/// The retired per-repo write (ADR-0071 §10, owner default 3) answers 410
/// and names where each thing it used to write went.
async fn assert_per_repo_write_gone(app: &axum::Router, method: &str, name: &str) {
    let (status, msg) = send(
        app,
        method,
        &format!("/v1/repos/acme/web/environments/{name}"),
        (method == "PUT").then(|| {
            serde_json::json!({
                "approvers": [], "wait_timer": 0, "allowed_refs": [], "concurrency": 1,
            })
        }),
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{method} {name}: {msg}");
    for pointer in [
        format!("/v1/orgs/acme/environments/{name}"),
        format!("/v1/orgs/acme/environments/{name}/eligibility"),
        format!("/v1/orgs/acme/environments/{name}/projects/web/overlay"),
        format!("/v1/orgs/acme/environments/{name}/projects/web/privilege-grant"),
    ] {
        assert!(
            msg.contains(&pointer),
            "410 body must point at {pointer}: {msg}"
        );
    }
}

/// Create the org Environment `name` (permissive baseline) and make `web`
/// eligible for it, through the API.
async fn create_environment_for_web(app: &axum::Router, name: &str) {
    let (status, msg) = send(
        app,
        "PUT",
        &format!("/v1/orgs/acme/environments/{name}"),
        Some(serde_json::json!({ "deployment_sources": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");
    let (status, msg) = send(
        app,
        "PUT",
        &format!("/v1/orgs/acme/environments/{name}/eligibility"),
        Some(serde_json::json!({ "all_projects": false, "projects": ["web"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");
}

/// Was: put / get / list / delete over `/v1/repos/{org}/{repo}/environments`.
/// Since ADR-0071 the writes are 410 and the same management happens on the
/// org Environment and the Project's overlay and grant; the repo-scoped GETs
/// (one, list, legacy history) are what they always were — reads — now of
/// what governs the Project.
#[tokio::test]
async fn environment_management_crud_over_repo_scoped_routes() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = app(pg.clone());

    // The retired writes: 410 with the pointer, and nothing created.
    assert_per_repo_write_gone(&app, "PUT", "prod").await;
    assert_per_repo_write_gone(&app, "DELETE", "prod").await;
    assert!(pg
        .get_org_environment("acme", "prod")
        .await
        .unwrap()
        .is_none());

    // The replacement for "PUT prod + staging under repo acme/web": org
    // Environments `web` is eligible for, and `web`'s own rules for prod as
    // its overlay.
    create_environment_for_web(&app, "prod").await;
    create_environment_for_web(&app, "staging").await;
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/prod/projects/web/overlay",
        Some(serde_json::json!({
            "approvers": ["anonymous"], "allowed_refs": ["refs/heads/main"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    // A Project the Environment does not admit has nothing to tighten.
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/prod/projects/api/overlay",
        Some(serde_json::json!({ "approvers": ["anonymous"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{msg}");
    assert!(msg.contains("eligible"), "{msg}");

    // GET one: baseline, overlay and the effective rules admission enforces.
    let (status, body) = send(&app, "GET", "/v1/repos/acme/web/environments/prod", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let prod = json_of(&body);
    assert_eq!(
        prod["overlay"]["approvers"],
        serde_json::json!(["anonymous"])
    );
    assert_eq!(
        prod["effective"]["approvers"],
        serde_json::json!(["anonymous"])
    );
    assert_eq!(
        prod["effective"]["allowed_ref_layers"],
        serde_json::json!([[], ["refs/heads/main"]])
    );

    // LIST returns both, alphabetical.
    let (status, body) = send(&app, "GET", "/v1/repos/acme/web/environments", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names = |body: &str| -> Vec<String> {
        json_of(body)
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(names(&body), vec!["prod", "staging"]);

    // Deployment (approval) history starts empty — the legacy read stays.
    let (status, body) = send(
        &app,
        "GET",
        "/v1/repos/acme/web/environments/prod/deployments",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(json_of(&body), serde_json::json!([]));

    // The privileges the per-repo rules carried are an org admin's grant
    // now; an entry that can never match an image is refused.
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/staging/projects/web/privilege-grant",
        Some(serde_json::json!({
            "privileged_images": [{ "image_digest": "latest", "privileged": true }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{msg}");
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/staging/projects/web/privilege-grant",
        Some(serde_json::json!({
            "privileged_images": [{ "image_digest": "sha256:abc123", "privileged": true }],
            "permit_k8s_overlay": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");
    let (_, body) = send(&app, "GET", "/v1/repos/acme/web/environments/staging", None).await;
    let staging = json_of(&body);
    assert_eq!(staging["privilege_grant"]["permit_k8s_overlay"], true);
    assert_eq!(
        staging["privilege_grant"]["privileged_images"][0]["image_digest"],
        "sha256:abc123"
    );

    // Deleting staging: refused while `web` holds a grant there (an
    // Environment deletion never drops a Project's privileges silently)…
    let (status, msg) = send(&app, "DELETE", "/v1/orgs/acme/environments/staging", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{msg}");
    // …then, with the grant withdrawn, it goes, and it's gone from the list.
    let (status, msg) = send(
        &app,
        "DELETE",
        "/v1/orgs/acme/environments/staging/projects/web/privilege-grant",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{msg}");
    let (status, msg) = send(&app, "DELETE", "/v1/orgs/acme/environments/staging", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{msg}");
    assert!(pg
        .get_org_environment("acme", "staging")
        .await
        .unwrap()
        .is_none());
    let (_, body) = send(&app, "GET", "/v1/repos/acme/web/environments", None).await;
    assert_eq!(names(&body), vec!["prod"]);

    // Removing `web`'s overlay leaves prod governed by its baseline alone.
    let overlay = "/v1/orgs/acme/environments/prod/projects/web/overlay";
    let (status, _) = send(&app, "DELETE", overlay, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, "DELETE", overlay, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "already gone");
    let (_, body) = send(&app, "GET", "/v1/repos/acme/web/environments/prod", None).await;
    assert_eq!(json_of(&body)["overlay"], serde_json::Value::Null);

    tdb.cleanup().await;
}

/// Was: the per-repo PUT accepted the UI form's body without `secret_scope`
/// / `oidc_subject` and stamped both from the `(org, repo, name)` path, so
/// callers could not inject a bogus scope. Under ADR-0071 neither is stored
/// at all: the overlay body has no such fields (sending one is refused, not
/// ignored), and the EnvironmentProjectSecretScope a Deploy Run resolves is
/// derived from the coordinates by the server — pinned here through the
/// secret matrix, which reads `web`'s `prod` column at exactly
/// `EnvironmentProject { acme, web, prod }`.
#[tokio::test]
async fn put_environment_stamps_canonical_scope_and_subject_when_body_omits_them() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let secrets = FakeSecrets::new().with_secret(
        &SecretScope::EnvironmentProject {
            org: "acme".into(),
            project: "web".into(),
            environment: "prod".into(),
        },
        "DEPLOY_KEY",
        b"k",
    );
    let secrets: Arc<dyn SecretProvider> = Arc::new(secrets);
    let app = router(app_state(pg.clone()).with_secrets(secrets));

    // The old route, with exactly the body the old env form sent.
    assert_per_repo_write_gone(&app, "PUT", "prod").await;

    create_environment_for_web(&app, "prod").await;
    // Exactly the body the new "Tighten for this project" form sends — no
    // scope, no subject.
    let overlay = "/v1/orgs/acme/environments/prod/projects/web/overlay";
    let (status, msg) = send(
        &app,
        "PUT",
        overlay,
        Some(serde_json::json!({
            "approvers": ["anonymous"],
            "wait_timer_s": 0,
            "allowed_refs": ["refs/heads/main"],
            "require_reason": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");

    // A caller cannot supply a scope or a subject: the fields do not exist,
    // and an unknown field is a 4xx, never silently dropped.
    for injected in [
        serde_json::json!({ "secret_scope": { "Org": { "org": "evil" } } }),
        serde_json::json!({ "oidc_subject": "scarab:org/evil/repo/x/env/prod" }),
    ] {
        let (status, msg) = send(&app, "PUT", overlay, Some(injected)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{msg}");
    }

    // The server derived the canonical scope from the coordinates.
    let (status, body) = send(&app, "GET", "/v1/repos/acme/web/secrets/matrix", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let m = json_of(&body);
    let rows = m["keys"].as_array().unwrap();
    let key = rows.iter().find(|r| r["key"] == "DEPLOY_KEY").unwrap();
    assert_eq!(key["status"]["prod"], "set", "{m}");

    tdb.cleanup().await;
}

/// ADR-0057 §3: `require_reason` persists and round-trips, and defaults
/// **off** (fail-open) when a body omits it. Was: through the per-repo PUT.
/// Now the same holds on both layers that carry it — a Project's overlay
/// and the Environment's baseline — and the effective rules (what admission
/// enforces) hold it when either layer sets it.
#[tokio::test]
async fn require_reason_persists_and_defaults_off_through_the_api() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = app(pg.clone());

    assert_per_repo_write_gone(&app, "PUT", "prod").await;
    create_environment_for_web(&app, "legacy").await;
    create_environment_for_web(&app, "prod").await;

    // An overlay body that OMITS require_reason → stored off.
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/legacy/projects/web/overlay",
        Some(serde_json::json!({ "approvers": [], "allowed_refs": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");
    let stored = pg
        .deploy_governance("acme", "web", "legacy")
        .await
        .unwrap()
        .expect("environment stored");
    assert!(
        !stored.overlay.expect("overlay stored").rules.require_reason,
        "an omitted require_reason defaults off (fail-open)"
    );

    // An overlay that SETS it → persists, reads back, and is effective.
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/prod/projects/web/overlay",
        Some(serde_json::json!({ "require_reason": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");
    let (status, body) = send(&app, "GET", "/v1/repos/acme/web/environments/prod", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let env = json_of(&body);
    assert_eq!(env["overlay"]["require_reason"], true, "round-trips: {env}");
    assert_eq!(env["effective"]["require_reason"], true, "{env}");
    assert_eq!(env["baseline"]["require_reason"], false, "{env}");

    // The baseline: omitted → off; set → persists and is effective for a
    // Project with no overlay of its own.
    let protection = "/v1/orgs/acme/environments/legacy/protection";
    let (status, msg) = send(&app, "PUT", protection, Some(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{msg}");
    let (_, body) = send(&app, "GET", protection, None).await;
    assert_eq!(json_of(&body)["require_reason"], false, "{body}");
    let (status, msg) = send(
        &app,
        "PUT",
        protection,
        Some(serde_json::json!({ "require_reason": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");
    let (_, body) = send(&app, "GET", protection, None).await;
    assert_eq!(json_of(&body)["require_reason"], true, "{body}");
    let (_, body) = send(&app, "GET", "/v1/repos/acme/web/environments/legacy", None).await;
    assert_eq!(
        json_of(&body)["effective"]["require_reason"],
        true,
        "{body}"
    );

    tdb.cleanup().await;
}

/// ADR-0071 §10 step 8 through the API: a name two Projects defined is one
/// report line naming both, each with its legacy rules beside its overlay
/// now, the rule they disagreed on, and the permissive baseline flagged
/// until an admin sets one. Acknowledging stamps the first reviewer only.
#[tokio::test]
async fn split_report_shows_merged_rules_and_acknowledges_once() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    // Two pre-migration per-repo `prod`s, then the 0057 backfill over them.
    for (project, approver) in [("web", "alice"), ("api", "bob")] {
        pg.put_environment(
            "acme",
            project,
            &scarab_project::Environment {
                name: "prod".into(),
                protection: rules(&[approver], &["refs/heads/main"]),
            },
        )
        .await
        .unwrap();
    }
    sqlx::query("SELECT adr0071_backfill()")
        .execute(&tdb.pool)
        .await
        .unwrap();
    let app = app(pg.clone());

    let (status, body) = send(
        &app,
        "GET",
        "/v1/orgs/acme/environment-migration-report",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let report = json_of(&body);
    let line = &report[0];
    assert_eq!(line["environment"], "prod", "{report}");
    assert_eq!(line["merged_projects"], serde_json::json!(["api", "web"]));
    assert_eq!(line["differing_rules"], serde_json::json!(["approvers"]));
    assert_eq!(line["permissive_baseline"], true);
    assert_eq!(line["acknowledged_at"], serde_json::Value::Null);
    let web = line["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["project"] == "web")
        .unwrap();
    assert_eq!(web["legacy"]["approvers"], serde_json::json!(["alice"]));
    assert_eq!(web["overlay"]["approvers"], serde_json::json!(["alice"]));

    let ack = "/v1/orgs/acme/environment-migration-report/prod/acknowledge";
    let (status, body) = send(&app, "POST", ack, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(json_of(&body)["acknowledged_by"], "anonymous");
    let first_at = json_of(&body)["acknowledged_at"].clone();
    assert_ne!(first_at, serde_json::Value::Null);
    let (_, body) = send(&app, "POST", ack, None).await;
    assert_eq!(json_of(&body)["acknowledged_at"], first_at, "first stands");
    let (status, _) = send(
        &app,
        "POST",
        "/v1/orgs/acme/environment-migration-report/nope/acknowledge",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Setting a baseline clears the warning.
    let (status, msg) = send(
        &app,
        "PUT",
        "/v1/orgs/acme/environments/prod/protection",
        Some(serde_json::json!({ "allowed_refs": ["refs/heads/main"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{msg}");
    let (_, body) = send(
        &app,
        "GET",
        "/v1/orgs/acme/environment-migration-report",
        None,
    )
    .await;
    assert_eq!(json_of(&body)[0]["permissive_baseline"], false, "{body}");

    tdb.cleanup().await;
}

/// Drive a deploy run to its manual gate, then approve it via HTTP. The gate
/// releases (and a deployment is recorded) only when the environment's rules
/// admit the accumulated approver; otherwise the run stays suspended.
async fn drive_to_gate(pg: &Arc<PostgresDb>, run: &RunId) {
    let (a, g, b) = (
        StepId("a".into()),
        StepId("gate".into()),
        StepId("b".into()),
    );
    let spec = StepSpec {
        image: "busybox".into(),
        command: vec!["true".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        clone: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    };
    pg.create_run(run, 1, 1, Timestamp(0)).await.unwrap();
    pg.create_step_run(run, &a, Some(&spec), &[], Timestamp(0))
        .await
        .unwrap();
    pg.create_step_run(run, &g, None, std::slice::from_ref(&a), Timestamp(0))
        .await
        .unwrap();
    pg.set_step_gate(run, &g, "manual", None).await.unwrap();
    pg.create_step_run(run, &b, Some(&spec), std::slice::from_ref(&g), Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded); // A
    let sched = Scheduler::new(pg.as_ref(), &clock, &exec, "sched");
    sched.tick(run).await.unwrap(); // run A
    sched.tick(run).await.unwrap(); // reach gate, suspend
    assert_eq!(
        pg.run_status(run).await.unwrap(),
        Some(RunStatus::Suspended)
    );
}

#[tokio::test]
async fn deploy_gate_releases_and_records_history_only_when_admitted() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = app(pg.clone());

    let ctx = DeployContext {
        org: "acme".into(),
        project: "web".into(),
        environment: "prod".into(),
        git_ref: "refs/heads/main".into(),
        locked_out: false,
    };
    let approve = |id: &str| {
        json_req(
            "POST",
            &format!("/v1/runs/{id}/gates/gate/approve"),
            serde_json::json!({}),
        )
    };

    // --- Case 1: approver requirement NOT met by the anonymous approver ---
    // (ADR-0071: `web`'s legacy rules, as the migration leaves them.)
    seed_migrated_environment(
        &pg,
        "acme",
        "web",
        "prod",
        rules(&["alice"], &["refs/heads/main"]), // requires alice, not anonymous
    )
    .await;
    let run1 = RunId("deploy-blocked".into());
    drive_to_gate(&pg, &run1).await;
    pg.set_run_deploy_context(&run1, &ctx).await.unwrap();

    let resp = app
        .clone()
        .oneshot(approve("deploy-blocked"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        pg.run_status(&run1).await.unwrap(),
        Some(RunStatus::Suspended),
        "unmet approver → gate stays closed"
    );
    assert!(pg
        .deployments("acme", "web", "prod")
        .await
        .unwrap()
        .is_empty());

    // --- Case 2: approver requirement satisfied by the anonymous approver ---
    seed_migrated_environment(
        &pg,
        "acme",
        "web",
        "prod",
        rules(&["anonymous"], &["refs/heads/main"]),
    )
    .await;
    let run2 = RunId("deploy-ok".into());
    drive_to_gate(&pg, &run2).await;
    pg.set_run_deploy_context(&run2, &ctx).await.unwrap();

    let resp = app.clone().oneshot(approve("deploy-ok")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        pg.run_status(&run2).await.unwrap(),
        Some(RunStatus::Running),
        "admitted → gate released, run resumes"
    );
    assert_eq!(
        pg.steps_of_run(&run2)
            .await
            .unwrap()
            .iter()
            .find(|s| s.step.0 == "gate")
            .unwrap()
            .status,
        StepStatus::Succeeded
    );
    let history = pg.deployments("acme", "web", "prod").await.unwrap();
    assert_eq!(history.len(), 1, "the admitted deploy is recorded once");
    assert_eq!(history[0].git_ref, "refs/heads/main");
    assert_eq!(history[0].approved_by, vec!["anonymous".to_string()]);

    tdb.cleanup().await;
}

/// The advisory secret parity matrix reports effective status per environment
/// (ADR-0037): a key defined at the repo scope is `inherited` by every env; a
/// key defined only at one env's scope is `set` there and `unset` elsewhere.
#[tokio::test]
async fn secret_matrix_reports_effective_status() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    for name in ["prod", "staging"] {
        seed_migrated_environment(&pg, "acme", "web", name, rules(&[], &[])).await;
    }
    // SHARED lives once at repo scope; PROD_ONLY only at prod's env scope.
    let secrets = FakeSecrets::new()
        .with_secret(
            &SecretScope::Repo {
                org: "acme".into(),
                repo: "web".into(),
            },
            "SHARED",
            b"x",
        )
        .with_secret(
            &SecretScope::EnvironmentProject {
                org: "acme".into(),
                project: "web".into(),
                environment: "prod".into(),
            },
            "PROD_ONLY",
            b"y",
        );
    let db: Arc<dyn Db> = pg.clone();
    let envs: Arc<dyn EnvironmentStore> = pg.clone();
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    let secrets: Arc<dyn SecretProvider> = Arc::new(secrets);
    let app = router(
        AppState::new(db, clock, logs)
            .with_environments(envs)
            .with_environment_governance(pg.clone())
            .with_secrets(secrets),
    );

    let resp = app
        .oneshot(get_req("/v1/repos/acme/web/secrets/matrix"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let m: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(m["environments"], serde_json::json!(["prod", "staging"]));
    // The repo-scope default is the leftmost column (ADR-0060) — the thing the
    // environments fall through to, addressed by the reserved empty id.
    assert_eq!(m["columns"], serde_json::json!(["", "prod", "staging"]));
    let rows = m["keys"].as_array().unwrap();
    let find = |k: &str| rows.iter().find(|r| r["key"] == k).unwrap();
    // Defined once at repo scope: set THERE, inherited everywhere else — and the
    // cells name what they'd be overriding.
    assert_eq!(
        find("SHARED")["status"],
        serde_json::json!({ "": "set", "prod": "inherited", "staging": "inherited" })
    );
    assert_eq!(
        find("SHARED")["inherited_from"],
        serde_json::json!({ "prod": "repo", "staging": "repo" })
    );
    // Defined at one environment only: the repo default is genuinely empty.
    assert_eq!(
        find("PROD_ONLY")["status"],
        serde_json::json!({ "": "unset", "prod": "set", "staging": "unset" })
    );
    assert_eq!(find("PROD_ONLY")["inherited_from"], serde_json::json!({}));

    tdb.cleanup().await;
}
