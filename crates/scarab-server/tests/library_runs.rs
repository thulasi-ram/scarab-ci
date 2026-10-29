//! ADR-0076 §4, §6: Runs of Pipelines that invoke an installed library.
//! Compiling resolves `lib:` against the org's installations and records the
//! version in `origin`; a yanked version is refused at Run creation, at
//! Step launch (Reruns and Retries included) and at gate release.
//!
//! Real admission, router and executor decorator; in-memory Db, forge and
//! stores.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::ports::{ExecState, FailureClass};
use scarab_engine::{Clock, Db, FailureKind, RunId, RunStatus, Scheduler, StepId, StepStatus};
use scarab_forge::{Event, RepoRef};
use scarab_policy::library::{
    content_hash, LibraryStore, LibraryVersion, LibraryYank, NewInstallation, Replaces,
};
use scarab_server::{
    dispatch_run, router, trigger_runs_from_event, AdmissionPorts, AppState, DispatchError,
    DispatchKind, LogService, SecretInjectingExecutor, TriggerError,
};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, FakeSecrets, InMemoryDb, InMemoryLibraries,
    InMemoryObjectStore, InMemoryPolicies,
};

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

const DEPLOY: &str = "kind: library\nsteps:\n  - id: helm\n    image: alpine/helm@sha256:aaaa\n";

const CI_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: test, image: busybox }
  - { id: ship, invoke: lib:acme/deploy, needs: [test] }
"#;

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "ship it".into(),
    }
}

fn forge(ci: &str) -> FakeForge {
    FakeForge::new()
        .with_file(".scarab/ci.yaml", ci)
        .with_commit("refs/heads/main", SHA)
}

fn source() -> String {
    format!("github.com/acme/libs//deploy.yaml@{}", "1".repeat(40))
}

async fn installed(org: &str, content: &str) -> InMemoryLibraries {
    let libs = InMemoryLibraries::new();
    libs.put_version(&LibraryVersion {
        org: org.into(),
        content_hash: content_hash(content),
        content: content.into(),
        source: source(),
        publisher: "acme".into(),
        name: "deploy".into(),
        first_party: false,
        fetched_by: "amy".into(),
        fetched_at: 1,
    })
    .await
    .unwrap();
    libs.install(
        &NewInstallation {
            org: org.into(),
            publisher: "acme".into(),
            name: "deploy".into(),
            content_hash: content_hash(content),
            installed_by: "amy".into(),
            installed_at: 1,
            step_up_ref: None,
        },
        &Replaces::Nothing,
    )
    .await
    .unwrap();
    libs
}

async fn push_with(
    db: &dyn Db,
    libs: &dyn LibraryStore,
    forge: &FakeForge,
) -> Result<scarab_server::TriggerOutcome, TriggerError> {
    trigger_runs_from_event(
        forge,
        db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(libs),
        &push(),
    )
    .await
}

fn origin_hashes(ir: &serde_json::Value, step: &str) -> Vec<String> {
    ir["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == step)
        .unwrap()["origin"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["content_hash"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_lib_target_resolves_to_the_orgs_installed_version() {
    let db = InMemoryDb::new();
    let libs = installed("acme", DEPLOY).await;
    let out = push_with(&db, &libs, &forge(CI_YAML)).await.unwrap();
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let ir = db.run_ir(&out.runs[0]).await.unwrap().unwrap();
    assert_eq!(origin_hashes(&ir, "ship/helm"), vec![content_hash(DEPLOY)]);
    let link = &ir["steps"].as_array().unwrap()[1]["origin"][0];
    assert_eq!(link["kind"], "library");
    assert_eq!(link["name"], "acme/deploy");
    assert_eq!(link["source"], source());
}

#[tokio::test]
async fn a_lib_target_not_installed_in_this_org_does_not_compile() {
    let db = InMemoryDb::new();
    // Installed in another org only.
    let libs = installed("globex", DEPLOY).await;
    let err = push_with(&db, &libs, &forge(CI_YAML)).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("`lib:acme/deploy` is not installed in this org"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Yank (ADR-0076 §6)
// ---------------------------------------------------------------------------

const REASON: &str = "pins scarab-clone with CVE-2026-1234";

async fn yank(libs: &dyn LibraryStore, content: &str) {
    libs.yank(&LibraryYank {
        org: "acme".into(),
        content_hash: content_hash(content),
        reason: REASON.into(),
        yanked_by: "amy".into(),
        yanked_at: 2,
    })
    .await
    .unwrap();
}

fn assert_names_the_yank(text: &str) {
    let short = &content_hash(DEPLOY)[..12];
    assert!(text.contains("lib:acme/deploy"), "{text}");
    assert!(text.contains(short), "names the version: {text}");
    assert!(text.contains(REASON), "quotes the reason: {text}");
}

/// Run creation refuses a yanked version with no Policy stored: the yank
/// is checked before (and without) the Policies.
#[tokio::test]
async fn run_create_refuses_a_yanked_version_with_no_policy_stored() {
    let db = InMemoryDb::new();
    let libs = installed("acme", DEPLOY).await;
    yank(&libs, DEPLOY).await;
    let policies = InMemoryPolicies::new();
    let out = trigger_runs_from_event(
        &forge(CI_YAML),
        &db,
        &FakeClock::new(1_000),
        AdmissionPorts::default()
            .with_libraries(&libs)
            .with_policies(&policies),
        &push(),
    )
    .await
    .unwrap();
    assert!(out.runs.is_empty(), "no Run for a yanked version");
    assert_eq!(out.refused.len(), 1);
    assert_names_the_yank(&out.refused[0].reason);

    // A dispatch is refused 403-style with the same text.
    let dispatch_yaml = CI_YAML.replace("push: {}", "manual: {}");
    let err = dispatch_run(
        &forge(&dispatch_yaml),
        &db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(&libs),
        "bob".into(),
        repo(),
        "refs/heads/main".into(),
        "ci".into(),
        Default::default(),
        DispatchKind::Manual,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, DispatchError::LibraryYanked(_)), "{err}");
    assert_names_the_yank(&err.to_string());

    // A yank applies to one org: the same version in another org runs.
    let globex = installed("globex", DEPLOY).await;
    let other = RepoRef {
        owner: "globex".into(),
        name: "web".into(),
    };
    let out = trigger_runs_from_event(
        &forge(CI_YAML),
        &db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(&globex),
        &Event::Push {
            actor: "octocat".into(),
            actor_id: String::new(),
            repo: other,
            r#ref: "refs/heads/main".into(),
            after: SHA.into(),
            message: "ship it".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
}

const SHIP_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: ship, invoke: lib:acme/deploy }
"#;

/// A Run created before the yank, its Step launched by the real launch
/// decorator over a fake backend.
async fn pre_yank_run(outcome: ExecState) -> (Arc<InMemoryDb>, Arc<InMemoryLibraries>, RunId) {
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(installed("acme", DEPLOY).await);
    let out = push_with(db.as_ref(), libs.as_ref(), &forge(SHIP_YAML))
        .await
        .unwrap();
    let run = out.runs[0].clone();
    let inner = Arc::new(FakeExecutor::new());
    inner.script_outcome(outcome);
    let exec = executor(&db, &libs, inner);
    let clock = FakeClock::new(1_000);
    let sched = Scheduler::new(db.as_ref(), &clock as &dyn Clock, &exec, "sched");
    for _ in 0..4 {
        sched.tick(&run).await.unwrap();
    }
    (db, libs, run)
}

fn executor(
    db: &Arc<InMemoryDb>,
    libs: &Arc<InMemoryLibraries>,
    inner: Arc<FakeExecutor>,
) -> SecretInjectingExecutor {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    SecretInjectingExecutor::new(
        inner,
        db.clone() as Arc<dyn Db>,
        Arc::new(FakeSecrets::new()),
        logs,
    )
    .with_libraries(libs.clone())
}

/// Ticks `run` after a rerun or retry, then the newest attempt of
/// `ship/helm` must be a Config failure naming the yank, with no launch.
async fn assert_launch_refused(db: &Arc<InMemoryDb>, libs: &Arc<InMemoryLibraries>, run: &RunId) {
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(db, libs, inner.clone());
    let clock = FakeClock::new(5_000);
    let sched = Scheduler::new(db.as_ref(), &clock as &dyn Clock, &exec, "sched");
    for _ in 0..4 {
        sched
            .tick(run)
            .await
            .expect("a refusal is a step verdict, never a tick error");
    }
    let step = StepId("ship/helm".into());
    let attempts = db.attempts_of_step(run, &step).await.unwrap();
    let last = attempts.last().unwrap();
    assert_eq!(last.failure, Some(FailureKind::Config), "{attempts:?}");
    assert_names_the_yank(last.failure_detail.as_deref().unwrap_or_default());
    let handle =
        scarab_engine::ports::ExecHandle(format!("fake://{}/ship/helm/{}", run.0, last.id.0));
    assert_eq!(
        inner.launch_count(&handle),
        0,
        "the backend was never asked to launch"
    );
    assert_eq!(db.run_status(run).await.unwrap(), Some(RunStatus::Failed));
}

#[tokio::test]
async fn a_rerun_of_a_run_from_before_the_yank_is_refused_at_launch() {
    let (db, libs, run) = pre_yank_run(ExecState::Succeeded).await;
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    yank(libs.as_ref(), DEPLOY).await;
    scarab_engine::rerun_step(
        db.as_ref(),
        &FakeClock::new(4_000),
        &run,
        &StepId("ship/helm".into()),
        Some("amy".into()),
    )
    .await
    .unwrap();
    assert_launch_refused(&db, &libs, &run).await;
}

#[tokio::test]
async fn a_retry_of_a_run_from_before_the_yank_is_refused_at_launch() {
    let (db, libs, run) = pre_yank_run(ExecState::Failed {
        exit_code: Some(1),
        class: FailureClass::Step,
        cause: None,
    })
    .await;
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Failed));
    yank(libs.as_ref(), DEPLOY).await;
    scarab_engine::retry_step(
        db.as_ref(),
        &FakeClock::new(4_000),
        &run,
        &StepId("ship/helm".into()),
        Some("amy".into()),
    )
    .await
    .unwrap();
    assert_launch_refused(&db, &libs, &run).await;
}

const GATED: &str = "kind: library\nsteps:\n  - { id: approve, gate: manual }\n  - { id: helm, image: alpine/helm@sha256:aaaa, needs: [approve] }\n";

/// A gate inlined from a yanked version neither takes an approval nor
/// releases, in a Run created before the yank.
#[tokio::test]
async fn a_gate_from_a_yanked_version_does_not_release() {
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(installed("acme", GATED).await);
    let out = push_with(db.as_ref(), libs.as_ref(), &forge(SHIP_YAML))
        .await
        .unwrap();
    let run = out.runs[0].clone();
    let exec = FakeExecutor::new();
    let clock = Arc::new(FakeClock::new(1_000));
    let sched = Scheduler::new(db.as_ref(), clock.as_ref() as &dyn Clock, &exec, "sched");
    for _ in 0..4 {
        sched.tick(&run).await.unwrap();
    }
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Suspended)
    );
    yank(libs.as_ref(), GATED).await;

    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db.clone(), clock, logs).with_libraries(libs.clone()));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/runs/{}/gates/ship%2Fapprove/approve", run.0))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains(REASON), "{body}");
    let gate = db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == "ship/approve")
        .unwrap();
    assert_ne!(gate.status, StepStatus::Succeeded);
    // No approval was recorded.
    let approved = db
        .events(&run)
        .await
        .unwrap()
        .iter()
        .any(|e| format!("{e:?}").contains("GateApproved"));
    assert!(!approved, "a refused approval is not recorded");
}
