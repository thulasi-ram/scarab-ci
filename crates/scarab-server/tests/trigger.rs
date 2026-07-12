//! In-repo config → run-on-trigger acceptance (ADR-0009, 0010): "commit a file,
//! done." A push whose `.scarab` `on:push` matches starts a run; a push to a ref
//! the trigger filters out does not; no config means no run. Hermetic — a
//! FakeForge serves the in-repo file, InMemoryDb is the store (no network).

use std::sync::Arc;

use scarab_engine::{ConcurrencyPolicy, Db, StepStatus};
use scarab_forge::{Event, Repo};
use scarab_server::trigger_run_from_event;
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb};

/// Runs one step, but only on a push to `main`.
const CI_YAML: &str = r#"
on:
  push:
    when: "event.branch == 'main'"
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

fn repo() -> Repo {
    Repo {
        owner: "acme".into(),
        name: "app".into(),
    }
}

fn push(branch: &str) -> Event {
    Event::Push {
        repo: repo(),
        r#ref: format!("refs/heads/{branch}"),
        after: "sha123".into(),
    }
}

async fn setup() -> (FakeForge, Arc<InMemoryDb>, Arc<FakeClock>) {
    let forge = FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML);
    (forge, Arc::new(InMemoryDb::new()), Arc::new(FakeClock::new(1_000)))
}

#[tokio::test]
async fn push_matching_on_push_starts_a_run() {
    let (forge, db, clock) = setup().await;

    let runs = trigger_run_from_event(&forge, db.as_ref(), clock.as_ref(), &push("main"))
        .await
        .expect("trigger");
    assert_eq!(runs.len(), 1, "one pipeline matched");
    let run = runs.into_iter().next().unwrap();

    // The run exists and was populated from the compiled config.
    assert!(db.run_status(&run).await.unwrap().is_some());
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].step.0, "build");
    assert_eq!(steps[0].status, StepStatus::Pending);
    // The IR was stored on the run (self-describing).
    assert!(db.run_ir(&run).await.unwrap().is_some());
}

/// A committed `.scarab` authoring slice-4 engine features: a concurrency group
/// and an image-less gate between two executed steps.
const CI_YAML_CONCURRENCY_GATE: &str = r#"
on:
  push: {}
concurrency:
  group: deploy-prod
  policy: cancel-in-progress
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: deploy, image: busybox, command: ["true"], needs: [approve] }
"#;

#[tokio::test]
async fn committed_scarab_authors_concurrency_and_gate() {
    let forge = FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML_CONCURRENCY_GATE);
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    let runs = trigger_run_from_event(&forge, db.as_ref(), clock.as_ref(), &push("main"))
        .await
        .expect("trigger");
    assert_eq!(runs.len(), 1, "one pipeline matched");
    let run = runs.into_iter().next().unwrap();

    // Concurrency reached the engine: group + policy are set on the run.
    let (group, policy) = db
        .run_concurrency(&run)
        .await
        .unwrap()
        .expect("run is in a concurrency group");
    assert_eq!(group, "deploy-prod");
    assert_eq!(policy, ConcurrencyPolicy::CancelInProgress);

    // The gate reached the engine: `approve` is a durable suspend point, the
    // other two are ordinary executed steps.
    let steps = db.steps_of_run(&run).await.unwrap();
    let approve = steps.iter().find(|s| s.step.0 == "approve").unwrap();
    assert!(approve.is_gate(), "approve should be a gate step");
    assert_eq!(approve.gate_kind.as_deref(), Some("manual"));
    assert!(
        steps.iter().find(|s| s.step.0 == "build").unwrap().gate_kind.is_none(),
        "build is an ordinary step"
    );
}

#[tokio::test]
async fn push_to_non_matching_ref_starts_no_run() {
    let (forge, db, clock) = setup().await;

    let runs = trigger_run_from_event(&forge, db.as_ref(), clock.as_ref(), &push("dev"))
        .await
        .expect("trigger");
    assert!(runs.is_empty(), "push to dev is filtered out by the on:push when");
    assert!(db.active_runs().await.unwrap().is_empty());
}

#[tokio::test]
async fn no_in_repo_config_starts_no_run() {
    // FakeForge with no seeded file → empty `.scarab/` listing → no run.
    let forge = FakeForge::new();
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    let runs = trigger_run_from_event(&forge, db.as_ref(), clock.as_ref(), &push("main"))
        .await
        .expect("trigger");
    assert!(runs.is_empty(), "no .scarab config → nothing to run");
}

/// Multiple pipelines under `.scarab/` each start their own run on a matching
/// trigger; a pipeline whose `on:` doesn't match is skipped.
#[tokio::test]
async fn multiple_pipelines_each_start_a_run() {
    let forge = FakeForge::new()
        .with_file(
            ".scarab/ci.yaml",
            "on: { push: {} }\nsteps: [{ id: build, image: busybox }]",
        )
        .with_file(
            ".scarab/nightly.yaml",
            "on: { push: {} }\nsteps: [{ id: bench, image: busybox }]",
        )
        // A deploy pipeline gated to tags — a push must NOT start it.
        .with_file(
            ".scarab/deploy.yaml",
            "on: { tag: {} }\nsteps: [{ id: ship, image: busybox }]",
        )
        // A non-YAML file in the directory is ignored, not compiled.
        .with_file(".scarab/README.md", "# pipelines");
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    let runs = trigger_run_from_event(&forge, db.as_ref(), clock.as_ref(), &push("main"))
        .await
        .expect("trigger");
    assert_eq!(runs.len(), 2, "ci + nightly match the push; deploy (tag) does not");

    // Collect the single step id of each started run to confirm which pipelines ran.
    let mut ran: Vec<String> = Vec::new();
    for run in &runs {
        let steps = db.steps_of_run(run).await.unwrap();
        ran.push(steps[0].step.0.clone());
    }
    ran.sort();
    assert_eq!(ran, vec!["bench".to_string(), "build".to_string()]);
}
