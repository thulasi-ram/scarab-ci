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

// Attack: a pull request adds many Pipeline files that each read
// `event.changed_paths`, to multiply the forge API calls one webhook costs.
// Safe outcome: one files request for the delivery, whatever the answer.
#[tokio::test]
async fn many_pipelines_reading_the_paths_cost_one_files_request_per_delivery() {
    let mut forge = FakeForge::new();
    for i in 0..20 {
        forge = forge.with_file(&format!(".scarab/p{i:02}.yaml"), CI_TOOLS);
    }
    let db = Arc::new(InMemoryDb::new());

    // The files request for #9 fails, so the paths stay unknown.
    forge.set_pr_files_unknown(9, Some("502 Bad Gateway"));
    assert_eq!(trigger(&forge, &db, &pr(9)).await.len(), 20);
    assert_eq!(
        forge.pr_files_calls(),
        1,
        "a failed request is not repeated"
    );

    forge.set_pr_files(10, &["README.md"]);
    assert!(trigger(&forge, &db, &pr(10)).await.is_empty());
    assert_eq!(forge.pr_files_calls(), 2);

    // A Pipeline that runs only on push and reads the paths in a step's
    // `when:`: its trigger does not match a pull request, so the delivery
    // asks for no files.
    let forge = FakeForge::new().with_file(".scarab/push-only.yaml", PUSH_ONLY_STEP_FILTER);
    forge.set_pr_files(11, &["docker/ci-tools/Dockerfile"]);
    assert!(trigger(&forge, &db, &pr(11)).await.is_empty());
    assert_eq!(forge.pr_files_calls(), 0);
}

/// Runs on push only; one step reads the paths.
const PUSH_ONLY_STEP_FILTER: &str = r#"
on:
  push: {}
steps:
  - id: image
    image: busybox
    command: ["true"]
    when: 'event.changed_paths.exists(p, p.startsWith("docker/ci-tools/"))'
"#;

#[tokio::test(start_paused = true)]
async fn a_files_request_that_does_not_answer_in_time_starts_the_run_with_a_note() {
    let forge = FakeForge::new().with_file(".scarab/ci-tools.yaml", CI_TOOLS);
    forge.set_pr_files(12, &["README.md"]);
    forge.set_pr_files_delay(std::time::Duration::from_secs(
        scarab_forge::PULL_REQUEST_FILES_TIMEOUT_SECS + 60,
    ));
    let db = Arc::new(InMemoryDb::new());

    let runs = trigger(&forge, &db, &pr(12)).await;
    assert_eq!(runs.len(), 1, "a slow forge leaves the paths unknown");
    let notes = notes(&db, &runs[0]).await;
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].contains(&PathsUnknown::TimedOut.reason()),
        "{notes:?}"
    );
}
