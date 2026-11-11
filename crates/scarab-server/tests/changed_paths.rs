//! `event.changed_paths` in a trigger's `when:` (git-bug 418f55e). A Pipeline
//! that builds an image only when `docker/ci-tools/` changes runs for a push
//! or pull request that touches it and not for one that touches only
//! `README.md`. When the paths are unknown (a new branch, a failed files
//! request), a `when:` that needs them counts as true and the Run's event
//! log says why. A pull request's files are asked of the forge only when a
//! Pipeline reads them. Hermetic: FakeForge, InMemoryDb.

use std::sync::Arc;

use scarab_engine::{Db, EventPayload, RunId};
use scarab_forge::{ChangedPaths, Event, PathsUnknown, RepoRef};
use scarab_server::trigger_run_from_event;
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb};

const CI_TOOLS: &str = r#"
on:
  push:
    when: 'event.changed_paths.exists(p, p.startsWith("docker/ci-tools/"))'
  pull_request:
    when: 'event.changed_paths.exists(p, p.startsWith("docker/ci-tools/"))'
steps:
  - { id: image, image: busybox, command: ["true"] }
"#;

/// Reads only the branch, never the paths.
const MAIN_ONLY: &str = r#"
on:
  push:
    when: "event.branch == 'main'"
  pull_request: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    }
}

fn push(branch: &str, changed_paths: ChangedPaths) -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        r#ref: format!("refs/heads/{branch}"),
        after: "abc123".into(),
        message: "a change".into(),
        changed_paths,
    }
}

fn known(paths: &[&str]) -> ChangedPaths {
    ChangedPaths::from_paths(paths.iter().map(|p| p.to_string()))
}

fn pr(number: u64) -> Event {
    Event::PullRequest {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        number,
        head: "def456".into(),
        title: "a change".into(),
        base: "main".into(),
        fork: false,
    }
}

async fn trigger(forge: &FakeForge, db: &InMemoryDb, event: &Event) -> Vec<RunId> {
    trigger_run_from_event(
        forge,
        db,
        &FakeClock::new(1_000),
        scarab_server::AdmissionPorts::governance(None),
        event,
    )
    .await
    .expect("trigger")
}

/// The `note` lines in a Run's event log.
async fn notes(db: &InMemoryDb, run: &RunId) -> Vec<String> {
    db.events(run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::Raw(v) => v["note"].as_str().map(str::to_string),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_push_runs_only_when_it_touches_the_filtered_directory() {
    let forge = FakeForge::new().with_file(".scarab/ci-tools.yaml", CI_TOOLS);
    let db = Arc::new(InMemoryDb::new());

    let readme = push("main", known(&["README.md"]));
    assert!(trigger(&forge, &db, &readme).await.is_empty());

    let dockerfile = push("main", known(&["README.md", "docker/ci-tools/Dockerfile"]));
    let runs = trigger(&forge, &db, &dockerfile).await;
    assert_eq!(runs.len(), 1);
    assert!(
        notes(&db, &runs[0]).await.is_empty(),
        "the filter was applied"
    );
}

#[tokio::test]
async fn a_pull_request_runs_only_when_its_files_touch_the_filtered_directory() {
    let forge = FakeForge::new().with_file(".scarab/ci-tools.yaml", CI_TOOLS);
    forge.set_pr_files(7, &["README.md"]);
    forge.set_pr_files(8, &["docker/ci-tools/Dockerfile"]);
    let db = Arc::new(InMemoryDb::new());

    assert!(trigger(&forge, &db, &pr(7)).await.is_empty());
    assert_eq!(trigger(&forge, &db, &pr(8)).await.len(), 1);
    assert_eq!(forge.pr_files_calls(), 2, "one files request per delivery");
}

#[tokio::test]
async fn unknown_paths_start_the_run_and_its_log_says_why() {
    let forge = FakeForge::new().with_file(".scarab/ci-tools.yaml", CI_TOOLS);
    let db = Arc::new(InMemoryDb::new());

    let new_branch = push("feat/x", ChangedPaths::Unknown(PathsUnknown::NewBranch));
    let runs = trigger(&forge, &db, &new_branch).await;
    assert_eq!(
        runs.len(),
        1,
        "a `when:` that needs unknown paths counts as true"
    );
    assert_eq!(
        notes(&db, &runs[0]).await,
        vec![
            "changed paths unknown: new branch; the path filter in `when:` was not applied \
             (on.push)"
                .to_string()
        ]
    );
}

#[tokio::test]
async fn a_failed_files_request_starts_the_run_and_its_log_says_why() {
    let forge = FakeForge::new().with_file(".scarab/ci-tools.yaml", CI_TOOLS);
    forge.set_pr_files_unknown(9, Some("502 Bad Gateway"));
    let db = Arc::new(InMemoryDb::new());

    let runs = trigger(&forge, &db, &pr(9)).await;
    assert_eq!(
        runs.len(),
        1,
        "an API error is unknown, not a refused trigger"
    );
    let notes = notes(&db, &runs[0]).await;
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].contains(&PathsUnknown::ForgeError.reason())
            && notes[0].contains("on.pull_request"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_when_that_does_not_read_the_paths_is_evaluated_as_before() {
    let forge = FakeForge::new().with_file(".scarab/ci.yaml", MAIN_ONLY);
    let db = Arc::new(InMemoryDb::new());

    let unknown = ChangedPaths::Unknown(PathsUnknown::NewBranch);
    assert!(trigger(&forge, &db, &push("feat/x", unknown.clone()))
        .await
        .is_empty());
    let runs = trigger(&forge, &db, &push("main", unknown)).await;
    assert_eq!(runs.len(), 1);
    assert!(notes(&db, &runs[0]).await.is_empty());
}

#[tokio::test]
async fn a_pipeline_that_does_not_read_the_paths_makes_no_files_request() {
    let forge = FakeForge::new().with_file(".scarab/ci.yaml", MAIN_ONLY);
    forge.set_pr_files(7, &["README.md"]);
    let db = Arc::new(InMemoryDb::new());

    assert_eq!(trigger(&forge, &db, &pr(7)).await.len(), 1);
    assert_eq!(forge.pr_files_calls(), 0);
}
