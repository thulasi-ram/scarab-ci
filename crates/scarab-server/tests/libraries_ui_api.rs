//! The API the ADR-0076 screens read: a yank's impact on Runs in progress
//! and the opt-in cancel; refused Run creations kept for the Runs list; a
//! Step's origin in the Run API; and a compile failure's errors with their
//! fields. Real router and admission; in-memory Db, forge and library store.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_engine::{
    Clock, Db, EventPayload, RunId, RunStatus, Scheduler, StepStatus,
};
use scarab_engine::ports::ExecState;
use scarab_forge::{Event, RepoRef};
use scarab_policy::library::{
    content_hash, LibraryStore, LibraryVersion, LibraryYank, NewInstallation, Replaces,
};
use scarab_server::{router, trigger_runs_from_event, AdmissionPorts, AppState, LogService};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, InMemoryDb, InMemoryLibraries, InMemoryObjectStore,
};

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

const DEPLOY: &str = r#"kind: library
interface:
  inputs:
    - { name: chart, type: string, required: false, default: ./chart }
    - { name: release, type: string }
steps:
  - id: helm
    image: alpine/helm@sha256:aaaa
    command: [helm, upgrade, "${{ inputs.chart }}"]
    env:
      - [RELEASE, "${{ inputs.release }}"]
"#;

const CI_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: test, image: busybox }
  - id: ship
    invoke: lib:acme/deploy
    with: { release: "${{ event.sha }}" }
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
        message: "Bump chart to 1.8.2".into(),
    }
}

fn forge(ci: &str) -> FakeForge {
    FakeForge::new()
        .with_file(".scarab/ci.yaml", ci)
        .with_commit("refs/heads/main", SHA)
        .with_commit("main", SHA)
}

fn source() -> String {
    format!("github.com/acme/libs//deploy.yaml@{}", "1".repeat(40))
}

async fn installed(content: &str) -> InMemoryLibraries {
    let libs = InMemoryLibraries::new();
    libs.put_version(&LibraryVersion {
        org: "acme".into(),
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
            org: "acme".into(),
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

async fn yank(libs: &dyn LibraryStore) {
    libs.yank(&LibraryYank {
        org: "acme".into(),
        content_hash: content_hash(DEPLOY),
        reason: "writes the token to the log".into(),
        yanked_by: "amy".into(),
        yanked_at: 2,
    })
    .await
    .unwrap();
}

struct H {
    app: axum::Router,
    db: Arc<InMemoryDb>,
    libs: Arc<InMemoryLibraries>,
    forge: Arc<FakeForge>,
}

async fn harness(ci: &str) -> H {
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(installed(DEPLOY).await);
    let forge = Arc::new(forge(ci));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let st = AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
        .with_forge(forge.clone())
        .with_libraries(libs.clone());
    H {
        app: router(st),
        db,
        libs,
        forge,
    }
}

async fn push_run(h: &H) -> scarab_server::TriggerOutcome {
    trigger_runs_from_event(
        h.forge.as_ref(),
        h.db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(h.libs.as_ref()),
        &push(),
    )
    .await
    .unwrap()
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, v)
}

/// Ticks `run` until `ship/helm` is running on a backend that never finishes.
async fn start(db: &InMemoryDb, run: &RunId) {
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Running);
    let clock = FakeClock::new(1_000);
    let sched = Scheduler::new(db, &clock as &dyn Clock, &exec, "sched");
    for _ in 0..6 {
        sched.tick(run).await.unwrap();
    }
}

#[tokio::test]
async fn the_impact_counts_runs_in_progress_and_a_yank_can_cancel_them() {
    let h = harness(CI_YAML).await;
    let run = push_run(&h).await.runs[0].clone();
    start(&h.db, &run).await;
    let helm = h
        .db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == "ship/helm")
        .unwrap();
    assert_eq!(helm.status, StepStatus::Running, "{helm:?}");

    let hash = content_hash(DEPLOY);
    let (st, v) = call(
        &h.app,
        "GET",
        &format!("/v1/orgs/acme/library-yanks/{hash}/impact"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["runs"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["runs"][0]["id"], run.0);
    assert_eq!(v["runs"][0]["project"], "web");
    assert_eq!(v["runs"][0]["steps"], json!(["ship/helm"]));
    assert_eq!(v["runs"][0]["running_steps"], json!(["ship/helm"]));
    assert_eq!(v["running_steps"], 1);

    // Another org's view of the same hash sees none of acme's Runs.
    let (_, other) = call(
        &h.app,
        "GET",
        &format!("/v1/orgs/globex/library-yanks/{hash}/impact"),
        None,
    )
    .await;
    assert_eq!(other["runs"], json!([]));

    let (st, y) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-yanks",
        Some(json!({"content_hash": hash, "reason": "writes the token to the log", "cancel_runs": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{y}");
    assert_eq!(y["cancelled_runs"], json!([run.0]));
    assert_eq!(
        h.db.run_status(&run).await.unwrap(),
        Some(RunStatus::Cancelled)
    );
    // Cancelled as an operator cancel is: journalled with who asked.
    let by: Vec<Option<String>> = h
        .db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::RunCancelRequested { by } => Some(by),
            _ => None,
        })
        .collect();
    assert_eq!(by.len(), 1, "{by:?}");
    assert!(by[0].is_some());

    // Nothing is in progress now.
    let (_, after) = call(
        &h.app,
        "GET",
        &format!("/v1/orgs/acme/library-yanks/{hash}/impact"),
        None,
    )
    .await;
    assert_eq!(after["runs"], json!([]));
}

#[tokio::test]
async fn a_yank_without_cancel_leaves_runs_running() {
    let h = harness(CI_YAML).await;
    let run = push_run(&h).await.runs[0].clone();
    start(&h.db, &run).await;
    let (st, y) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-yanks",
        Some(json!({"content_hash": content_hash(DEPLOY), "reason": "r"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{y}");
    assert!(y.get("cancelled_runs").is_none(), "{y}");
    assert_eq!(h.db.run_status(&run).await.unwrap(), Some(RunStatus::Running));
    let (st, bad) = call(&h.app, "GET", "/v1/orgs/acme/library-yanks/nothex/impact", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}");
}

#[tokio::test]
async fn the_run_api_shows_each_steps_origin_and_the_runs_yanked_versions() {
    let h = harness(CI_YAML).await;
    let run = push_run(&h).await.runs[0].clone();
    let (_, before) = call(&h.app, "GET", &format!("/v1/runs/{}", run.0), None).await;
    let step = |v: &Value, id: &str| {
        v["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .cloned()
            .unwrap()
    };
    assert!(step(&before, "test").get("origin").is_none());
    let link = &step(&before, "ship/helm")["origin"][0];
    assert_eq!(link["kind"], "library");
    assert_eq!(link["name"], "acme/deploy");
    assert_eq!(link["content_hash"], content_hash(DEPLOY));
    assert_eq!(link["source"], source());
    assert!(link.get("yanked").is_none());
    assert!(before.get("yanked_libraries").is_none());

    yank(h.libs.as_ref()).await;
    let (_, after) = call(&h.app, "GET", &format!("/v1/runs/{}", run.0), None).await;
    assert_eq!(step(&after, "ship/helm")["origin"][0]["yanked"], true);
    let y = &after["yanked_libraries"][0];
    assert_eq!(y["name"], "acme/deploy");
    assert_eq!(y["reason"], "writes the token to the log");
    assert_eq!(y["steps"], json!(["ship/helm"]));

    // The detail: the chain, the bound inputs, and no render before launch.
    let (st, o) = call(
        &h.app,
        "GET",
        &format!("/v1/runs/{}/steps/ship%2Fhelm/origin", run.0),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{o}");
    assert_eq!(o["chain"][0]["name"], "acme/deploy");
    assert_eq!(o["chain"][0]["yanked"], true);
    let inputs = o["inputs"].as_array().unwrap();
    let input = |n: &str| inputs.iter().find(|i| i["name"] == n).cloned().unwrap();
    assert_eq!(input("chart")["kind"], "literal");
    assert_eq!(input("chart")["bound"], "./chart");
    assert_eq!(input("release")["kind"], "template");
    assert_eq!(input("release")["bound"], "${{ event.sha }}");
    assert!(o.get("rendered").is_none());
    assert!(o["render_note"].as_str().unwrap().contains("launches"), "{o}");

    let (st, _) = call(
        &h.app,
        "GET",
        &format!("/v1/runs/{}/steps/nope/origin", run.0),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_launched_steps_origin_shows_it_rendered() {
    let h = harness(CI_YAML).await;
    let run = push_run(&h).await.runs[0].clone();
    start(&h.db, &run).await;
    let (st, o) = call(
        &h.app,
        "GET",
        &format!("/v1/runs/{}/steps/ship%2Fhelm/origin", run.0),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{o}");
    let r = &o["rendered"];
    assert_eq!(r["image"], "alpine/helm@sha256:aaaa");
    assert_eq!(r["command"], json!(["helm", "upgrade", "./chart"]));
    assert!(
        r["env"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p[0] == "RELEASE" && p[1] == SHA),
        "{r}"
    );
}

#[tokio::test]
async fn a_refused_push_is_kept_for_the_runs_list() {
    let h = harness(CI_YAML).await;
    yank(h.libs.as_ref()).await;
    let out = push_run(&h).await;
    assert!(out.runs.is_empty());

    let (st, v) = call(&h.app, "GET", "/v1/repos/acme/web/refused-runs", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let r = &v["refused"][0];
    assert_eq!(r["pipeline"], ".scarab/ci.yaml");
    assert_eq!(r["reason_kind"], "yanked");
    assert_eq!(r["trigger_kind"], "push");
    assert_eq!(r["actor"], "octocat");
    assert_eq!(r["sha"], SHA);
    assert_eq!(r["git_ref"], "refs/heads/main");
    assert_eq!(r["title"], "Bump chart to 1.8.2");
    assert!(r["message"].as_str().unwrap().contains("yanked"), "{r}");
    assert!(
        r["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("writes the token to the log"),
        "{r}"
    );

    // Another Project's list is its own.
    let (_, other) = call(&h.app, "GET", "/v1/repos/acme/api/refused-runs", None).await;
    assert_eq!(other["refused"], json!([]));
}

const FREE_TEXT: &str = r#"
interface:
  inputs:
    - { name: version, type: string }
on:
  push: {}
  manual: {}
steps:
  - id: tag
    image: alpine/git@sha256:aaaa
    command: [git, tag, -a, "${{ inputs.version }}"]
"#;

#[tokio::test]
async fn a_compile_failure_has_each_errors_fields_and_is_kept_on_push() {
    let h = harness(FREE_TEXT).await;
    let (st, v) = call(
        &h.app,
        "GET",
        "/v1/repos/acme/web/pipelines/ci/interface?ref=main",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["code"], "compile_failed");
    assert_eq!(v["sha"], SHA);
    assert!(v["message"].as_str().unwrap().starts_with("pipeline: "), "{v}");
    let e = &v["errors"][0];
    assert_eq!(e["kind"], "command_value");
    assert_eq!(e["step"], "tag");
    assert_eq!(e["command_index"], 3);
    assert_eq!(e["read"], "${{ inputs.version }}");
    assert!(e["message"]
        .as_str()
        .unwrap()
        .contains("Scarab puts no such value into a command"));

    // The same file on a push: the delivery fails as before, and the
    // refusal is kept with the same fields.
    let err = trigger_runs_from_event(
        h.forge.as_ref(),
        h.db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(h.libs.as_ref()),
        &push(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("command[3]"), "{err}");
    let kept = h.db.refused_runs_for_tenant("acme", "web", 10).await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].reason_kind, "compile");
    assert_eq!(kept[0].errors[0]["kind"], "command_value");
    assert_eq!(kept[0].errors[0]["step"], "tag");
}
