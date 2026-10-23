//! A DeploymentTarget belongs to exactly one Environment (CONTEXT §4.6), so a
//! Run that requests one homed elsewhere is refused **at Run creation**
//! (slice b, criterion 1; ADR-0070 §7) — before any Step runs, like every
//! other admission refusal: a dispatch answers 403 naming the home and what to
//! do, a webhook skips the pipeline, warns, and names it in the delivery's
//! `refused` list. Against real Postgres, so the home is read exactly as
//! production reads it: where the target's source routes NOW (a plugin's
//! report through an Environment's deployment source), else the Environment
//! of its newest input in the Deployment record. An admitted Run carries its
//! resolved requests (`${{ inputs.* }}` bound). Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_deployment::{
    DeploymentInput, DeploymentRequester, DeploymentStore, DeploymentTarget, DeploymentVersion,
};
use scarab_engine::{Db, StepId};
use scarab_forge::{Event, RepoRef};
use scarab_plugin::{
    InstallationState, PluginCapability, PluginInstallation, PluginManifest, PluginStore,
    ReportIngest, ReportItem, ReportedStatus, RuntimeStatus, StatusReportItem,
};
use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentDeploymentSource, EnvironmentGovernanceStore,
    EnvironmentProjectEligibility, OrgEnvironment, OrgEnvironmentStore,
};
use scarab_server::{
    dispatch_run, router, trigger_runs_from_event, AppState, DispatchError, DispatchKind,
    LogService,
};
use scarab_testkit::{FakeClock, FakeForge, InMemoryObjectStore};

const ORG: &str = "acme";
const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

fn repo() -> RepoRef {
    RepoRef {
        owner: ORG.into(),
        name: "web".into(),
    }
}

/// A deploy pipeline into `environment` whose `bump` step requests the
/// `argocd` target named by its `app` input (default `web`, so a webhook Run —
/// which supplies no parameters — requests `web` too).
fn deploy_yaml(environment: &str) -> String {
    format!(
        r#"
on:
  push: {{}}
  manual: {{}}
environment: {environment}
interface:
  inputs:
    - {{ name: app, type: string, required: false, default: web }}
steps:
  - id: bump
    image: busybox
    command: ["true"]
    results: [version]
    deployment_requests:
      - target: {{ source: argocd, id: "${{{{ inputs.app }}}}" }}
        version: version
"#
    )
}

fn forge(environment: &str) -> FakeForge {
    FakeForge::new()
        .with_file(".scarab/deploy.yaml", deploy_yaml(environment).as_str())
        .with_commit("refs/heads/main", SHA)
}

fn installation() -> PluginInstallation {
    PluginInstallation {
        id: "0000000001".into(),
        org: ORG.into(),
        name: "argocd".into(),
        plugin: "argocd".into(),
        image: format!("ghcr.io/acme/argocd@sha256:{}", "a".repeat(64)),
        manifest: PluginManifest {
            plugin: "argocd".into(),
            capabilities: vec![PluginCapability::ReportDeployments],
            connections: vec![],
            config_keys: vec![],
        },
        manifest_sha256: "0".repeat(64),
        config: BTreeMap::new(),
        state: InstallationState::Enabled,
        runtime_status: RuntimeStatus::Pending,
        runtime_message: String::new(),
        probe_passed_at: None,
        approved_by: "amy".into(),
        approved_at: 1_000,
        killed_by: None,
        killed_at: None,
    }
}

async fn put_environment(pg: &PostgresDb, name: &str, grouping: Option<&str>) {
    pg.put_org_environment(&OrgEnvironment {
        org: ORG.into(),
        name: name.into(),
        description: String::new(),
        deployment_deadline: EnvironmentDeploymentDeadline::new(5 * 60_000).unwrap(),
        deployment_sources: grouping
            .map(|g| EnvironmentDeploymentSource {
                installation: "argocd".into(),
                vendor_grouping: g.into(),
            })
            .into_iter()
            .collect(),
        protection: Default::default(),
        eligibility: EnvironmentProjectEligibility::AllProjects,
        created_at: 1_000,
        updated_at: 1_000,
        updated_by: "amy".into(),
    })
    .await
    .unwrap();
    // A create takes the closed default (ADR-0071 §2); open it explicitly.
    pg.put_environment_eligibility(
        ORG,
        name,
        &EnvironmentProjectEligibility::AllProjects,
        "amy",
        1_000,
    )
    .await
    .unwrap();
}

/// `prod` (whose `argocd` grouping `prod` has reported target `web`, so `web`
/// routes to `prod` now) and `staging` (no sources), both open to every
/// Project; and the record's last word on target `api` is a request from
/// `staging` (no routing for it at all).
async fn seeded() -> Option<(common::TestDb, Arc<PostgresDb>)> {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return None;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    pg.install(&installation()).await.unwrap();
    put_environment(&pg, "prod", Some("prod")).await;
    put_environment(&pg, "staging", None).await;
    let item = ReportItem::Status(StatusReportItem {
        idempotency_key: "k1".into(),
        sequence: 1,
        vendor_grouping: "prod".into(),
        target_id: "web".into(),
        version: "v1".into(),
        version_scheme: "argocd.target-images.v1".into(),
        status: ReportedStatus::Live,
        label: "synced".into(),
        link: None,
        source_at: 7,
        owners: None,
    });
    pg.ingest_report(
        "0000000001",
        &[ReportIngest {
            content_sha256: serde_json::to_string(&item).unwrap(),
            item,
        }],
        2_000,
    )
    .await
    .unwrap();
    pg.append(
        ORG,
        &DeploymentInput::Request {
            target: DeploymentTarget {
                environment: "staging".into(),
                source: "argocd".into(),
                id: "api".into(),
            },
            version: DeploymentVersion("a1".into()),
            requester: DeploymentRequester {
                run: "run-0".into(),
                step: "bump".into(),
            },
            effective_at: 2_000,
            at: 2_000,
            evidence: Vec::new(),
        },
        2_100,
    )
    .await
    .unwrap();
    Some((tdb, pg))
}

async fn dispatch(
    pg: &PostgresDb,
    environment: &str,
    app: &str,
) -> Result<scarab_engine::RunId, DispatchError> {
    dispatch_run(
        &forge(environment),
        pg,
        &FakeClock::new(1_000),
        Some(pg),
        "bob".into(),
        repo(),
        "refs/heads/main".into(),
        "deploy".into(),
        BTreeMap::from([("app".to_string(), json!(app))]),
        DispatchKind::Manual,
        None,
    )
    .await
}

/// Dispatch: a target homed elsewhere — by its routing (`web` → prod) or, with
/// no routing, by its newest input (`api` → staging) — is refused with no Run;
/// the same targets from their home, and a target nobody has seen, are
/// admitted, the Run carrying its request with the input bound.
#[tokio::test]
async fn dispatch_refuses_a_target_homed_in_another_environment() {
    let Some((tdb, pg)) = seeded().await else {
        return;
    };

    let err = dispatch(&pg, "staging", "web")
        .await
        .expect_err("`web` routes to prod");
    match &err {
        DispatchError::DeploymentTargetElsewhere {
            environment,
            step,
            target_source,
            target_id,
            home,
        } => assert_eq!(
            (
                environment.as_str(),
                step.as_str(),
                target_source.as_str(),
                target_id.as_str(),
                home.as_str()
            ),
            ("staging", "bump", "argocd", "web", "prod")
        ),
        other => panic!("expected DeploymentTargetElsewhere, got {other:?}"),
    }
    assert!(
        err.to_string().contains("belongs to environment `prod`"),
        "{err}"
    );
    let err = dispatch(&pg, "prod", "api")
        .await
        .expect_err("`api` was last recorded in staging");
    assert!(
        matches!(&err, DispatchError::DeploymentTargetElsewhere { home, .. } if home == "staging"),
        "{err:?}"
    );
    assert!(
        pg.list_runs(100).await.unwrap().is_empty(),
        "no Run was created"
    );

    // From home, and for a target nobody has seen: admitted.
    let run = dispatch(&pg, "prod", "web").await.unwrap();
    let requests = pg
        .run_deployment_requests(&run, &StepId("bump".into()))
        .await
        .unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        (
            requests[0].environment.as_str(),
            requests[0].target_source.as_str(),
            requests[0].target_id.as_str(),
            requests[0].version_result.as_str()
        ),
        ("prod", "argocd", "web", "version"),
        "the `${{ inputs.app }}` target is bound at Run creation"
    );
    dispatch(&pg, "staging", "fresh").await.unwrap();
    dispatch(&pg, "staging", "api").await.unwrap();
    tdb.cleanup().await;
}

/// Over HTTP the refusal is a 403 whose body names the home Environment.
#[tokio::test]
async fn dispatch_endpoint_answers_403_for_a_target_homed_elsewhere() {
    let Some((tdb, pg)) = seeded().await else {
        return;
    };
    let db: Arc<dyn Db> = pg.clone();
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    let forge: Arc<dyn scarab_forge::ForgePort> = Arc::new(forge("staging"));
    let app = router(
        AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
            .with_forge(forge)
            .with_environment_governance(pg.clone()),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/repos/acme/web/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "ref": "refs/heads/main", "pipeline": "deploy",
                            "params": { "app": "web" } })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        body.contains("`argocd`/`web`") && body.contains("belongs to environment `prod`"),
        "{body}"
    );
    assert!(pg.list_runs(100).await.unwrap().is_empty());
    tdb.cleanup().await;
}

/// Webhook: the deploy pipeline is skipped (no Run) and named in the
/// delivery's `refused` list with the Run's Environment and the reason.
#[tokio::test]
async fn webhook_skips_and_names_a_pipeline_requesting_a_target_homed_elsewhere() {
    let Some((tdb, pg)) = seeded().await else {
        return;
    };
    let push = Event::Push {
        actor: "octocat".into(),
        repo: repo(),
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "ship it".into(),
    };
    let outcome = trigger_runs_from_event(
        &forge("staging"),
        pg.as_ref(),
        &FakeClock::new(1_000),
        Some(pg.as_ref()),
        &push,
    )
    .await
    .unwrap();
    assert!(outcome.runs.is_empty(), "{outcome:?}");
    assert_eq!(outcome.refused.len(), 1, "{outcome:?}");
    assert_eq!(outcome.refused[0].pipeline, ".scarab/deploy.yaml");
    assert_eq!(outcome.refused[0].environment, "staging");
    assert!(
        outcome.refused[0].reason.contains(
            "requests DeploymentTarget `argocd`/`web`, which belongs to environment `prod`"
        ),
        "{:?}",
        outcome.refused
    );
    assert!(pg.list_runs(100).await.unwrap().is_empty());

    // From its home the same push deploys (the webhook Run binds the input's
    // default).
    let outcome = trigger_runs_from_event(
        &forge("prod"),
        pg.as_ref(),
        &FakeClock::new(1_000),
        Some(pg.as_ref()),
        &push,
    )
    .await
    .unwrap();
    assert_eq!(outcome.runs.len(), 1, "{outcome:?}");
    tdb.cleanup().await;
}
