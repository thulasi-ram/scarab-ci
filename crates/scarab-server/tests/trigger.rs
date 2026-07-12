//! In-repo config → run-on-trigger acceptance (ADR-0009, 0010): "commit a file,
//! done." A push whose `.scarab` `on:push` matches starts a run; a push to a ref
//! the trigger filters out does not; no config means no run. Hermetic — a
//! FakeForge serves the in-repo file, InMemoryDb is the store (no network).

use std::sync::Arc;

use scarab_engine::{Db, StepStatus};
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

    let run = trigger_run_from_event(&forge, db.as_ref(), clock.as_ref(), &push("main"))
        .await
        .expect("trigger");
    let run = run.expect("a matching push starts a run");

    // The run exists and was populated from the compiled config.
    assert!(db.run_status(&run).await.unwrap().is_some());
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].step.0, "build");
    assert_eq!(steps[0].status, StepStatus::Pending);
    // The IR was stored on the run (self-describing).
    assert!(db.run_ir(&run).await.unwrap().is_some());
}

#[tokio::test]
async fn push_to_non_matching_ref_starts_no_run() {
    let (forge, db, clock) = setup().await;

    let run = trigger_run_from_event(&forge, db.as_ref(), clock.as_ref(), &push("dev"))
        .await
        .expect("trigger");
    assert!(run.is_none(), "push to dev is filtered out by the on:push when");
    assert!(db.active_runs().await.unwrap().is_empty());
}

#[tokio::test]
async fn no_in_repo_config_starts_no_run() {
    // FakeForge with no seeded file → read_file_at_ref errors → no run.
    let forge = FakeForge::new();
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    let run = trigger_run_from_event(&forge, db.as_ref(), clock.as_ref(), &push("main"))
        .await
        .expect("trigger");
    assert!(run.is_none(), "no .scarab config → nothing to run");
}
