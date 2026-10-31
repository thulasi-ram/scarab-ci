//! ADR-0071 §10's promise, end to end: **every Run the migration can see is
//! admitted and governed exactly as before.**
//!
//! A corpus of legacy per-Project environments is seeded into the
//! pre-0057 schema on real Postgres, migration 0057 runs, and then the real
//! server paths — the webhook trigger, manual dispatch, step privilege
//! admission and gate approval — decide every case against the migrated org
//! Environments. Each decision is compared with the one the **legacy
//! `ProtectionRules`** of that row make, applied exactly as the pre-ADR-0071
//! server applied them (the oracle functions below quote it). Covered:
//! corpus row × {push main, push feature, PR, tag, comment, cron, manual
//! ± reason} × grant requests × approval sets — plus a Project that only RAN
//! a deploy into another Project's name, and the two never-seen cases the
//! migration cannot preserve (a never-run referencer, a never-run name).
//!
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_engine::ports::ExecState;
use scarab_engine::{
    Db, DeployContext, RunId, RunStatus, Scheduler, StepId, StepSpec, StepStatus, Timestamp,
};
use scarab_forge::{Event, RepoRef};
use scarab_project::{GrantRequest, ProtectionRules};
use scarab_server::{
    dispatch_run, router, trigger_run_from_event, AppState, DispatchError, DispatchKind, LogService,
};
use scarab_testkit::{FakeClock, FakeExecutor, FakeForge, InMemoryObjectStore};

const ORG: &str = "acme";

/// `(project, environment, legacy protection JSON)` — one row per Project so
/// each keeps its own overlay; the JSON in the shapes real rows hold.
fn corpus() -> Vec<(&'static str, &'static str, serde_json::Value)> {
    use serde_json::json;
    vec![
        // Pre-ADR-0039/0055/0057 row: none of the newer keys.
        (
            "web",
            "prod",
            json!({
                "approvers": ["alice", "bob"], "wait_timer": 0,
                "allowed_refs": ["refs/heads/main"], "concurrency": 1,
                "secret_scope": { "Environment": { "org": "acme", "repo": "web", "environment": "prod" } },
                "oidc_subject": ""
            }),
        ),
        // A bare `main`, require_reason, and a whitelist — merged into `prod`.
        (
            "api",
            "prod",
            json!({
                "approvers": [], "wait_timer": 0, "allowed_refs": ["main"], "concurrency": 1,
                "secret_scope": { "Org": { "org": "" } }, "oidc_subject": "",
                "privileged_images": [
                    { "image_digest": "sha256:aaaa", "privileged": true, "capabilities": ["NET_ADMIN"] }
                ],
                "require_reason": true
            }),
        ),
        // Empty refs (any ref, and refless events), raw overlays permitted.
        (
            "svc",
            "staging",
            json!({
                "approvers": [], "wait_timer": 0, "allowed_refs": [], "concurrency": 1,
                "secret_scope": { "Org": { "org": "" } }, "oidc_subject": "",
                "permit_k8s_overlay": true
            }),
        ),
        // Tags only; an approver listed twice.
        (
            "lib",
            "release",
            json!({
                "approvers": ["carol", "carol"], "wait_timer": 0,
                "allowed_refs": ["refs/tags/v*"], "concurrency": 0,
                "secret_scope": { "Org": { "org": "" } }, "oidc_subject": "",
                "privileged_images": [], "permit_k8s_overlay": false, "require_reason": false
            }),
        ),
        // Branches and PRs, require_reason, a capability-only whitelist.
        (
            "ops",
            "canary",
            json!({
                "approvers": ["alice"], "wait_timer": 0,
                "allowed_refs": ["refs/heads/*", "refs/pull/*"], "concurrency": 1,
                "secret_scope": { "Org": { "org": "" } }, "oidc_subject": "",
                "privileged_images": [
                    { "image_digest": "sha256:bbbb", "capabilities": ["SYS_ADMIN"] }
                ],
                "require_reason": true
            }),
        ),
    ]
}

/// Seed the corpus into the pre-0057 schema, then run 0057.
async fn migrated_corpus(pool: &sqlx::PgPool) {
    for m in MIGRATOR.iter().filter(|m| m.version < 57) {
        sqlx::raw_sql(&m.sql).execute(pool).await.unwrap();
    }
    for (project, name, protection) in corpus() {
        sqlx::query(
            "INSERT INTO environments (org, project, name, protection) VALUES ($1, $2, $3, $4)",
        )
        .bind(ORG)
        .bind(project)
        .bind(name)
        .bind(protection)
        .execute(pool)
        .await
        .unwrap();
    }
    // Run history: `edge` deployed into `canary` (which `ops` defines)
    // without defining it — the cross-project referencer (c-full review L3).
    sqlx::query(
        "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at,
                           updated_at, deploy_org, deploy_project, deploy_environment,
                           deploy_git_ref)
         VALUES ('r-edge-canary', 'succeeded', 1, 1, 0, 0, $1, 'edge', 'canary',
                 'refs/heads/main')",
    )
    .bind(ORG)
    .execute(pool)
    .await
    .unwrap();
    let m = MIGRATOR.iter().find(|m| m.version == 57).unwrap();
    sqlx::raw_sql(&m.sql).execute(pool).await.unwrap();
    for m in MIGRATOR.iter().filter(|m| m.version > 57) {
        sqlx::raw_sql(&m.sql).execute(pool).await.unwrap();
    }
}

fn legacy(json: &serde_json::Value) -> ProtectionRules {
    serde_json::from_value(json.clone()).expect("a legacy row deserialises")
}

fn repo(project: &str) -> RepoRef {
    RepoRef {
        owner: ORG.into(),
        name: project.into(),
    }
}

// --- the pre-ADR-0071 decisions, quoted from the old `admit_and_create_run` --

/// Old run-creation ref check: a symbolic ref must be allowed; an event
/// without one is admitted only by an unrestricted environment.
fn legacy_admits_ref(p: &ProtectionRules, protection_ref: Option<&str>) -> bool {
    match protection_ref {
        Some(r) => p.ref_allowed(r),
        None => p.allowed_refs.is_empty(),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DispatchOutcome {
    Created,
    RefNotAllowed,
    ReasonRequired,
}

/// Old dispatch decision: the ref first, then `require_reason`.
fn legacy_dispatch(p: &ProtectionRules, sym_ref: &str, has_reason: bool) -> DispatchOutcome {
    if !legacy_admits_ref(p, Some(sym_ref)) {
        DispatchOutcome::RefNotAllowed
    } else if p.admits_reason(true, has_reason).is_err() {
        DispatchOutcome::ReasonRequired
    } else {
        DispatchOutcome::Created
    }
}

fn deploy_yaml(environment: &str) -> String {
    format!(
        "on:\n  push: {{}}\n  pull_request: {{}}\n  tag: {{}}\n  comment: {{}}\n  manual: {{}}\n\
         environment: {environment}\n\
         steps:\n  - {{ id: ship, image: busybox, command: [\"true\"] }}\n"
    )
}

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

fn webhook_events(project: &str) -> Vec<(&'static str, Event)> {
    let repo = repo(project);
    vec![
        (
            "push main",
            Event::Push {
                actor: "octocat".into(),
                actor_id: String::new(),
                repo: repo.clone(),
                r#ref: "refs/heads/main".into(),
                after: SHA.into(),
                message: "m".into(),
            },
        ),
        (
            "push feature",
            Event::Push {
                actor: "octocat".into(),
                actor_id: String::new(),
                repo: repo.clone(),
                r#ref: "refs/heads/feature".into(),
                after: SHA.into(),
                message: "m".into(),
            },
        ),
        (
            "pull request",
            Event::PullRequest {
                actor: "octocat".into(),
                actor_id: String::new(),
                repo: repo.clone(),
                number: 7,
                head: SHA.into(),
                title: "t".into(),
                base: "main".into(),
                fork: false,
            },
        ),
        (
            "tag",
            Event::Tag {
                actor: "octocat".into(),
                actor_id: String::new(),
                repo: repo.clone(),
                tag: "v1".into(),
            },
        ),
        (
            "comment (no symbolic ref)",
            Event::Comment {
                actor: "octocat".into(),
                actor_id: String::new(),
                repo,
                issue: 7,
                body: "deploy".into(),
            },
        ),
    ]
}

#[tokio::test]
async fn migrated_governance_admits_and_refuses_every_run_exactly_as_the_legacy_rules() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    migrated_corpus(&tdb.pool).await;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    let clock = FakeClock::new(1_000);

    for (project, environment, json) in corpus() {
        assert_decides_as_legacy(&pg, &clock, project, environment, &legacy(&json)).await;
    }

    tdb.cleanup().await;
}

/// Every webhook event, cron, and manual dispatch ± reason for `project`'s
/// deploy into `environment` decides on the migrated governance exactly as
/// the legacy rules `old` decided it.
async fn assert_decides_as_legacy(
    pg: &Arc<PostgresDb>,
    clock: &FakeClock,
    project: &str,
    environment: &str,
    old: &ProtectionRules,
) {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", deploy_yaml(environment).as_str())
        .with_commit("refs/heads/main", SHA);

    // Webhook events: created iff the old ref check admitted it.
    for (label, event) in webhook_events(project) {
        let before = legacy_admits_ref(old, event.protection_ref().as_deref());
        let runs = trigger_run_from_event(
            &forge,
            pg.as_ref(),
            clock,
            scarab_server::AdmissionPorts::governance(Some(pg.as_ref())),
            &event,
        )
        .await
        .unwrap_or_else(|e| panic!("{project}/{environment} {label}: {e}"));
        assert_eq!(
            runs.len(),
            usize::from(before),
            "{project}/{environment} {label}: admitted before = {before}"
        );
    }

    // Cron is repo-less: it never reaches admission, before or after.
    let cron = Event::Cron {
        schedule: "0 * * * *".into(),
    };
    assert!(trigger_run_from_event(
        &forge,
        pg.as_ref(),
        clock,
        scarab_server::AdmissionPorts::governance(Some(pg.as_ref())),
        &cron
    )
    .await
    .unwrap()
    .is_empty());

    // Manual dispatch ± reason, on a branch, a tag and a PR ref.
    for (dispatch_ref, sym_ref) in [
        ("main", "refs/heads/main"),
        ("refs/tags/v1", "refs/tags/v1"),
        ("refs/pull/7/head", "refs/pull/7/head"),
    ] {
        for reason in [None, Some("ship it".to_string())] {
            let before = legacy_dispatch(old, sym_ref, reason.is_some());
            let after = match dispatch(
                pg,
                clock,
                project,
                environment,
                dispatch_ref,
                reason.clone(),
            )
            .await
            {
                Ok(_) => DispatchOutcome::Created,
                Err(DispatchError::RefNotAllowed(_)) => DispatchOutcome::RefNotAllowed,
                Err(DispatchError::ReasonRequired(_)) => DispatchOutcome::ReasonRequired,
                Err(other) => panic!("{project}/{environment} {dispatch_ref}: {other:?}"),
            };
            assert_eq!(
                after, before,
                "{project}/{environment} dispatch {dispatch_ref} reason={reason:?}"
            );
        }
    }
}

/// Manual dispatch of `project`'s deploy pipeline into `environment`.
async fn dispatch(
    pg: &Arc<PostgresDb>,
    clock: &FakeClock,
    project: &str,
    environment: &str,
    dispatch_ref: &str,
    reason: Option<String>,
) -> Result<RunId, DispatchError> {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", deploy_yaml(environment).as_str())
        .with_commit("refs/heads/main", SHA);
    dispatch_run(
        &forge,
        pg.as_ref(),
        clock,
        scarab_server::AdmissionPorts::governance(Some(pg.as_ref())),
        "bob".into(),
        repo(project),
        dispatch_ref.into(),
        "deploy".into(),
        BTreeMap::new(),
        DispatchKind::Manual,
        reason,
    )
    .await
}

/// The cases the ADR-0071 migration can and cannot see beyond the defining
/// Projects (c-full review F1/L3), pinned so the behaviour change is explicit:
///
/// - `edge` never defined `canary` (`ops` did) but RAN a deploy into it: the
///   migration saw that Run, so `edge` is eligible and decides exactly as it
///   did before — with no rules at all (the permissive baseline, no overlay).
/// - `fresh` references `canary` too but never ran: the migration could not
///   see its YAML, so it is NOT eligible — refused where d4 admitted it.
/// - `nightly` was named by no Project and no Run: there is no org
///   Environment, so it is UnknownEnvironment — refused where d4 admitted it.
#[tokio::test]
async fn a_run_referencer_keeps_its_permissive_deploy_and_never_seen_cases_are_refused() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    migrated_corpus(&tdb.pool).await;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    let clock = FakeClock::new(1_000);

    // d4 admitted a Project with no definition as "no rules": no approvers,
    // any ref (including refless events), no reason required.
    let no_rules = legacy(&serde_json::json!({
        "approvers": [], "wait_timer": 0, "allowed_refs": [], "concurrency": 0,
        "secret_scope": { "Org": { "org": "" } }, "oidc_subject": ""
    }));
    assert_decides_as_legacy(&pg, &clock, "edge", "canary", &no_rules).await;

    let err = dispatch(&pg, &clock, "fresh", "canary", "main", Some("x".into()))
        .await
        .expect_err("a never-run referencer of a defined name is not eligible");
    assert!(matches!(err, DispatchError::NotEligible { .. }), "{err:?}");

    let err = dispatch(&pg, &clock, "web", "nightly", "main", Some("x".into()))
        .await
        .expect_err("a never-run, never-defined name has no Environment");
    assert!(
        matches!(err, DispatchError::UnknownEnvironment { .. }),
        "{err:?}"
    );

    // The webhook path refuses both too: the deploy pipeline starts no Run.
    for (project, environment) in [("fresh", "canary"), ("web", "nightly")] {
        let forge = FakeForge::new()
            .with_file(".scarab/deploy.yaml", deploy_yaml(environment).as_str())
            .with_commit("refs/heads/main", SHA);
        let (_, push) = webhook_events(project).swap_remove(0);
        let runs = trigger_run_from_event(
            &forge,
            pg.as_ref(),
            &clock,
            scarab_server::AdmissionPorts::governance(Some(pg.as_ref())),
            &push,
        )
        .await
        .unwrap();
        assert!(runs.is_empty(), "{project}/{environment}: refused, no Run");
    }

    tdb.cleanup().await;
}

/// A step's privilege request and raw overlay are decided by the migrated
/// grant exactly as by the legacy whitelist / `permit_k8s_overlay`.
#[tokio::test]
async fn migrated_privilege_grants_decide_every_request_as_the_legacy_whitelist() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    migrated_corpus(&tdb.pool).await;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    let clock = FakeClock::new(1_000);

    let requests: Vec<(&str, &str, GrantRequest)> = vec![
        (
            "busybox",
            "{ run_as_root: true }",
            GrantRequest {
                run_as_root: true,
                ..Default::default()
            },
        ),
        (
            "ghcr.io/acme/x@sha256:aaaa",
            "{ privileged: true }",
            GrantRequest {
                privileged: true,
                ..Default::default()
            },
        ),
        (
            "ghcr.io/acme/x@sha256:aaaa",
            "{ add_capabilities: [NET_ADMIN] }",
            GrantRequest {
                add_capabilities: vec!["NET_ADMIN".into()],
                ..Default::default()
            },
        ),
        (
            "ghcr.io/acme/x@sha256:bbbb",
            "{ add_capabilities: [SYS_ADMIN] }",
            GrantRequest {
                add_capabilities: vec!["SYS_ADMIN".into()],
                ..Default::default()
            },
        ),
        (
            "ghcr.io/acme/x:v1",
            "{ privileged: true }",
            GrantRequest {
                privileged: true,
                ..Default::default()
            },
        ),
    ];

    for (project, environment, json) in corpus() {
        let old = legacy(&json);
        // A dispatch ref this row's old rules admit; a row admitting none (a
        // bare `main`) never reached grant admission before or after.
        let Some((dispatch_ref, _)) = [
            ("main", "refs/heads/main"),
            ("refs/tags/v1", "refs/tags/v1"),
            ("refs/pull/7/head", "refs/pull/7/head"),
        ]
        .into_iter()
        .find(|(_, sym)| old.ref_allowed(sym)) else {
            continue;
        };
        let dispatch = |yaml: String| {
            let pg = pg.clone();
            let clock = &clock;
            async move {
                let forge = FakeForge::new().with_file(".scarab/grant.yaml", yaml.as_str());
                dispatch_run(
                    &forge,
                    pg.as_ref(),
                    clock,
                    scarab_server::AdmissionPorts::governance(Some(pg.as_ref())),
                    "bob".into(),
                    repo(project),
                    dispatch_ref.into(),
                    "grant".into(),
                    BTreeMap::new(),
                    DispatchKind::Manual,
                    Some("grant check".into()),
                )
                .await
            }
        };
        let header = format!("on: {{ manual: {{}} }}\nenvironment: {environment}\nsteps:\n");

        for (image, security, req) in &requests {
            let before = old.admit_grants(req, image, false).is_ok();
            let yaml =
                format!("{header}  - {{ id: s, image: \"{image}\", security: {security} }}\n");
            match dispatch(yaml).await {
                Ok(_) => assert!(
                    before,
                    "{project}/{environment} {image} {security}: now admitted"
                ),
                Err(DispatchError::Pipeline(m)) => {
                    assert!(m.contains("privilege request rejected"), "{m}");
                    assert!(
                        !before,
                        "{project}/{environment} {image} {security}: now refused"
                    );
                }
                Err(other) => panic!("{project}/{environment}: {other:?}"),
            }
        }

        let yaml = format!(
            "{header}  - id: s\n    image: busybox\n    k8s_overlay:\n      spec:\n        schedulerName: x\n"
        );
        match dispatch(yaml).await {
            Ok(_) => assert!(
                old.permit_k8s_overlay,
                "{project}/{environment}: overlay now admitted"
            ),
            Err(DispatchError::Pipeline(m)) => {
                assert!(m.contains("k8s_overlay"), "{m}");
                assert!(
                    !old.permit_k8s_overlay,
                    "{project}/{environment}: overlay now refused"
                );
            }
            Err(other) => panic!("{project}/{environment}: {other:?}"),
        }
    }

    tdb.cleanup().await;
}

/// A deploy suspended at its manual gate, with the given deploy context.
async fn run_at_gate(pg: &Arc<PostgresDb>, run: &RunId, ctx: &DeployContext) {
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
        forge_token: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
        scope: None,
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
    pg.set_run_deploy_context(run, ctx).await.unwrap();
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);
    let sched = Scheduler::new(pg.as_ref(), &clock, &exec, "sched");
    sched.tick(run).await.unwrap();
    sched.tick(run).await.unwrap();
    assert_eq!(
        pg.run_status(run).await.unwrap(),
        Some(RunStatus::Suspended)
    );
}

/// Gate approval: for every row × ref × set of prior approvals, the gate
/// releases now iff the legacy rules' `admits` passed before. (Test-mode
/// auth approves as `anonymous`, which no corpus rule names, so it is in the
/// approval set on both sides and changes nothing.)
#[tokio::test]
async fn migrated_gates_release_for_exactly_the_approvals_the_legacy_rules_required() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    migrated_corpus(&tdb.pool).await;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    let db: Arc<dyn Db> = pg.clone();
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    let app = router(
        AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
            .with_environments(pg.clone())
            .with_environment_governance(pg.clone()),
    );
    let clock = FakeClock::new(1_000);
    let people = ["alice", "bob", "carol"];

    let mut n = 0;
    for (project, environment, json) in corpus() {
        let old = legacy(&json);
        for git_ref in ["refs/heads/main", "refs/tags/v1", "main"] {
            for mask in 0..(1u32 << people.len()) {
                let prior: Vec<String> = people
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, p)| p.to_string())
                    .collect();
                let mut approvals = prior.clone();
                approvals.push("anonymous".into());
                let before = old.admits(git_ref, &approvals).is_ok();

                n += 1;
                let run = RunId(format!("gate-{n}"));
                run_at_gate(
                    &pg,
                    &run,
                    &DeployContext {
                        org: ORG.into(),
                        project: project.into(),
                        environment: environment.into(),
                        git_ref: git_ref.into(),
                        locked_out: false,
                    },
                )
                .await;
                for by in &prior {
                    scarab_engine::record_gate_approval(
                        pg.as_ref(),
                        &clock,
                        &run,
                        &StepId("gate".into()),
                        by,
                    )
                    .await
                    .unwrap();
                }
                let resp = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(format!("/v1/runs/{}/gates/gate/approve", run.0))
                            .header("content-type", "application/json")
                            .body(Body::from("{}"))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::ACCEPTED);
                let released = pg
                    .steps_of_run(&run)
                    .await
                    .unwrap()
                    .iter()
                    .any(|s| s.step.0 == "gate" && s.status == StepStatus::Succeeded);
                assert_eq!(
                    released, before,
                    "{project}/{environment} ref={git_ref} prior approvals={prior:?}"
                );
            }
        }
    }

    tdb.cleanup().await;
}
