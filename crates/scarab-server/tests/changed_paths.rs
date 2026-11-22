//! `event.changed_paths` in a trigger's `when:` (git-bug 418f55e, ADR-0081).
//! A Pipeline that builds an image only when `docker/ci-tools/` changes runs
//! for a push or pull request that touches it and not for one that touches
//! only `README.md`. A push's paths come from the forge's compare API and a
//! pull request's from its files API, each asked once per delivery and only
//! when a Pipeline reads them. When the paths are unknown (a new branch, a
//! failed or slow request), a `when:` that needs them counts as true and the
//! Run's event log says why. Hermetic: FakeForge, InMemoryDb.

use std::sync::Arc;

use scarab_engine::{Db, EventPayload, RunId, StepStatus};
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

const BEFORE: &str = "1111111111111111111111111111111111111111";
const AFTER: &str = "2222222222222222222222222222222222222222";

/// A push from `before` to `after` as normalize makes it: its paths not
/// asked for yet, so a Run that starts on a scripted compare answer proves
/// the compare was called with these two ids.
fn push(branch: &str, before: &str, after: &str) -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        r#ref: format!("refs/heads/{branch}"),
        before: before.into(),
        after: after.into(),
        message: "a change".into(),
        changed_paths: ChangedPaths::Unknown(PathsUnknown::NotFetched),
    }
}

/// A push whose payload already decided its paths: no request.
fn decided_push(branch: &str, changed_paths: ChangedPaths) -> Event {
    let Event::Push {
        actor,
        actor_id,
        repo,
        r#ref,
        before,
        after,
        message,
        ..
    } = push(branch, BEFORE, AFTER)
    else {
        unreachable!()
    };
    Event::Push {
        actor,
        actor_id,
        repo,
        r#ref,
        before,
        after,
        message,
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
async fn a_push_runs_only_when_the_forge_says_it_touched_the_directory() {
    let forge = FakeForge::new().with_file(".scarab/ci-tools.yaml", CI_TOOLS);
    let other = "3333333333333333333333333333333333333333";
    forge.set_push_paths(BEFORE, AFTER, known(&["docker/ci-tools/Dockerfile"]));
    forge.set_push_paths(AFTER, other, known(&["README.md"]));
    let db = Arc::new(InMemoryDb::new());

    let runs = trigger(&forge, &db, &push("main", BEFORE, AFTER)).await;
    assert_eq!(runs.len(), 1);
    assert!(
        notes(&db, &runs[0]).await.is_empty(),
        "the filter was applied"
    );
    assert!(trigger(&forge, &db, &push("main", AFTER, other))
        .await
        .is_empty());
    assert_eq!(forge.push_paths_calls(), 2, "one compare per delivery");
}

// Attack: a push adds many Pipeline files that each read
// `event.changed_paths`, to multiply the forge API calls one webhook costs.
// Safe outcome: one compare for the delivery, whatever the answer, and none
// for a delivery no Pipeline would ask for.
#[tokio::test]
async fn a_push_delivery_asks_once_however_many_pipelines_read_the_paths() {
    let mut forge = FakeForge::new();
    for i in 0..20 {
        forge = forge.with_file(&format!(".scarab/p{i:02}.yaml"), CI_TOOLS);
    }
    let db = Arc::new(InMemoryDb::new());
    // No answer scripted for the pair: the compare fails, the paths stay
    // unknown, and every Pipeline runs.
    assert_eq!(
        trigger(&forge, &db, &push("main", BEFORE, AFTER))
            .await
            .len(),
        20
    );
    assert_eq!(
        forge.push_paths_calls(),
        1,
        "a failed compare is not repeated"
    );

    let forge = FakeForge::new().with_file(".scarab/ci.yaml", MAIN_ONLY);
    assert_eq!(
        trigger(&forge, &db, &push("main", BEFORE, AFTER))
            .await
            .len(),
        1
    );
    assert_eq!(forge.push_paths_calls(), 0, "no Pipeline reads the paths");

    // The branch test decides first: on another branch the trigger cannot
    // match, so the paths are never needed.
    let forge = FakeForge::new().with_file(".scarab/main-tools.yaml", MAIN_TOOLS);
    assert!(trigger(&forge, &db, &push("feature", BEFORE, AFTER))
        .await
        .is_empty());
    assert_eq!(forge.push_paths_calls(), 0);
}

/// Runs on `main` only, and there only when `docker/ci-tools/` changed.
const MAIN_TOOLS: &str = r#"
on:
  push:
    when: 'event.branch == "main" && event.changed_paths.exists(p, p.startsWith("docker/ci-tools/"))'
steps:
  - { id: image, image: busybox, command: ["true"] }
"#;

/// Runs on every push; one step reads the paths.
const PUSH_STEP_FILTER: &str = r#"
on:
  push: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
  - id: image
    image: busybox
    command: ["true"]
    when: 'event.changed_paths.exists(p, p.startsWith("docker/ci-tools/"))'
"#;

#[tokio::test]
async fn a_step_when_reading_the_paths_asks_once_and_sees_the_answer() {
    let forge = FakeForge::new().with_file(".scarab/ci.yaml", PUSH_STEP_FILTER);
    forge.set_push_paths(BEFORE, AFTER, known(&["README.md"]));
    let db = Arc::new(InMemoryDb::new());

    let runs = trigger(&forge, &db, &push("main", BEFORE, AFTER)).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(forge.push_paths_calls(), 1);
    let image = db
        .steps_of_run(&runs[0])
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == "image");
    assert_eq!(
        image.map(|s| s.status),
        Some(StepStatus::Skipped),
        "the compare's answer reached the step's `when:`"
    );
    assert!(notes(&db, &runs[0]).await.is_empty());
}

#[tokio::test]
async fn a_reason_the_payload_decided_makes_no_request() {
    let forge = FakeForge::new().with_file(".scarab/ci-tools.yaml", CI_TOOLS);
    let db = Arc::new(InMemoryDb::new());

    let forced = decided_push("main", ChangedPaths::Unknown(PathsUnknown::Forced));
    let runs = trigger(&forge, &db, &forced).await;
    assert_eq!(
        runs.len(),
        1,
        "a `when:` that needs unknown paths counts as true"
    );
    assert_eq!(
        notes(&db, &runs[0]).await,
        vec![
            "changed paths unknown: forced push; the path filter in `when:` was not applied \
             (on.push)"
                .to_string()
        ]
    );
    // `before` equal to `after`: the push changed nothing.
    assert!(trigger(&forge, &db, &decided_push("main", known(&[])))
        .await
        .is_empty());
    assert_eq!(forge.push_paths_calls(), 0);
}

#[tokio::test]
async fn the_forges_reason_reaches_the_runs_log() {
    let forge = FakeForge::new().with_file(".scarab/ci-tools.yaml", CI_TOOLS);
    forge.set_push_paths(BEFORE, AFTER, ChangedPaths::Unknown(PathsUnknown::NotFound));
    let db = Arc::new(InMemoryDb::new());

    let runs = trigger(&forge, &db, &push("main", BEFORE, AFTER)).await;
    assert_eq!(runs.len(), 1);
    let notes = notes(&db, &runs[0]).await;
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].contains(&PathsUnknown::NotFound.reason()) && notes[0].contains("on.push"),
        "{notes:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_slow_compare_starts_the_runs_once_with_the_timed_out_note() {
    let forge = FakeForge::new()
        .with_file(".scarab/a.yaml", CI_TOOLS)
        .with_file(".scarab/b.yaml", CI_TOOLS);
    forge.set_push_paths(BEFORE, AFTER, known(&["README.md"]));
    let limit = scarab_forge::CHANGED_FILES_TIMEOUT_SECS;
    forge.set_push_paths_delay(std::time::Duration::from_secs(limit + 60));
    let db = Arc::new(InMemoryDb::new());

    let started = tokio::time::Instant::now();
    let runs = trigger(&forge, &db, &push("main", BEFORE, AFTER)).await;
    assert_eq!(runs.len(), 2, "a slow forge leaves the paths unknown");
    for run in &runs {
        let notes = notes(&db, run).await;
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains(&PathsUnknown::TimedOut.reason()),
            "{notes:?}"
        );
    }
    assert_eq!(forge.push_paths_calls(), 1);
    // A second wait would take the delivery past GitHub's 10 seconds.
    assert!(started.elapsed() < std::time::Duration::from_secs(2 * limit));
}

#[tokio::test]
async fn a_when_that_does_not_read_the_paths_is_evaluated_as_before() {
    let forge = FakeForge::new().with_file(".scarab/ci.yaml", MAIN_ONLY);
    let db = Arc::new(InMemoryDb::new());

    let unknown = ChangedPaths::Unknown(PathsUnknown::NewBranch);
    assert!(
        trigger(&forge, &db, &decided_push("feat/x", unknown.clone()))
            .await
            .is_empty()
    );
    let runs = trigger(&forge, &db, &decided_push("main", unknown)).await;
    assert_eq!(runs.len(), 1);
    assert!(notes(&db, &runs[0]).await.is_empty());
}

const BROKEN: &str = r#"
on:
  push: {}
steps:
  - { id: a, image: busybox, command: ["true"], needs: [ghost] }
"#;

/// A delivery with a file that does not compile decides every other
/// Pipeline's trigger to fail its check as not run (git-bug e5089fd), with
/// the same one compare.
#[tokio::test]
async fn a_broken_file_does_not_cost_a_second_compare() {
    for (paths, ci_tools_fails) in [
        (["docker/ci-tools/Dockerfile"], true),
        (["README.md"], false),
    ] {
        let forge = FakeForge::new()
            .with_file(".scarab/broken.yaml", BROKEN)
            .with_file(".scarab/ci-tools.yaml", CI_TOOLS)
            .with_file(".scarab/ci-tools-2.yaml", CI_TOOLS);
        forge.set_push_paths(BEFORE, AFTER, known(&paths));
        let db = Arc::new(InMemoryDb::new());
        let refused = trigger_run_from_event(
            &forge,
            db.as_ref(),
            &FakeClock::new(1_000),
            scarab_server::AdmissionPorts::governance(None),
            &push("main", BEFORE, AFTER),
        )
        .await;
        assert!(refused.is_err(), "a broken file stops the delivery");
        assert_eq!(forge.push_paths_calls(), 1, "{paths:?}");
        scarab_server::drain_forge_statuses(&forge, db.as_ref(), "test", 100, 0, "https://s/")
            .await
            .expect("drain");
        let failed: Vec<String> = forge
            .statuses_on(AFTER)
            .into_iter()
            .filter(|s| s.state == scarab_forge::StatusState::Failure)
            .map(|s| s.context)
            .filter(|c| c.starts_with("scarab/ci-tools"))
            .collect();
        assert_eq!(
            failed.len(),
            if ci_tools_fails { 2 } else { 0 },
            "{paths:?}: {failed:?}"
        );
    }
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
        scarab_forge::CHANGED_FILES_TIMEOUT_SECS + 60,
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

/// Runs only when the list is known and `docker/ci-tools/` changed
/// (`event.changed_paths_known`, ADR-0081 §6): an unknown list skips it.
const GUARDED: &str = r#"
on:
  push:
    when: 'event.changed_paths_known && event.changed_paths.exists(p, p.startsWith("docker/ci-tools/"))'
steps:
  - { id: image, image: busybox, command: ["true"] }
"#;

/// The bare Pipeline names of `runs`, sorted.
async fn pipelines(db: &InMemoryDb, runs: &[RunId]) -> Vec<String> {
    let mut out = Vec::new();
    for run in runs {
        out.push(db.run_pipeline(run).await.unwrap().unwrap_or_default());
    }
    out.sort();
    out
}

#[tokio::test]
async fn a_guarded_when_skips_on_an_unknown_list_and_a_plain_filter_still_runs() {
    let forge = || {
        FakeForge::new()
            .with_file(".scarab/guarded.yaml", GUARDED)
            .with_file(".scarab/ci-tools.yaml", CI_TOOLS)
    };

    // The forge answers that the list is unknown: the guard skips, the plain
    // filter counts as true and says why.
    let unknown = forge();
    unknown.set_push_paths(BEFORE, AFTER, ChangedPaths::Unknown(PathsUnknown::Forced));
    let db = Arc::new(InMemoryDb::new());
    let runs = trigger(&unknown, &db, &push("main", BEFORE, AFTER)).await;
    assert_eq!(pipelines(&db, &runs).await, vec!["ci-tools".to_string()]);
    let notes_a = notes(&db, &runs[0]).await;
    assert_eq!(notes_a.len(), 1, "{notes_a:?}");
    assert!(notes_a[0].contains("(on.push)"), "{notes_a:?}");
    assert_eq!(unknown.push_paths_calls(), 1);

    // The forge answers a known list that touches the directory: the flag
    // turned true when the answer was applied, so both run.
    let listed = forge();
    listed.set_push_paths(BEFORE, AFTER, known(&["docker/ci-tools/Dockerfile"]));
    let db = Arc::new(InMemoryDb::new());
    let runs = trigger(&listed, &db, &push("main", BEFORE, AFTER)).await;
    assert_eq!(
        pipelines(&db, &runs).await,
        vec!["ci-tools".to_string(), "guarded".to_string()]
    );
    for run in &runs {
        assert!(notes(&db, run).await.is_empty());
    }
    assert_eq!(listed.push_paths_calls(), 1);

    // The payload already decided the list is unknown: no request, and the
    // flag reads false.
    let decided = forge();
    let db = Arc::new(InMemoryDb::new());
    let new_branch = decided_push("main", ChangedPaths::Unknown(PathsUnknown::NewBranch));
    let runs = trigger(&decided, &db, &new_branch).await;
    assert_eq!(pipelines(&db, &runs).await, vec!["ci-tools".to_string()]);
    assert_eq!(decided.push_paths_calls(), 0);
}

/// Reads only `event.changed_paths_known`, never the list.
const FLAG_ONLY: &str = r#"
on:
  push:
    when: event.changed_paths_known
  pull_request:
    when: event.changed_paths_known
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

#[tokio::test]
async fn a_when_that_reads_only_the_flag_asks_the_forge() {
    let forge = FakeForge::new().with_file(".scarab/ci.yaml", FLAG_ONLY);
    forge.set_push_paths(BEFORE, AFTER, known(&["a"]));
    let db = Arc::new(InMemoryDb::new());
    assert_eq!(
        trigger(&forge, &db, &push("main", BEFORE, AFTER))
            .await
            .len(),
        1
    );
    assert_eq!(forge.push_paths_calls(), 1);

    forge.set_pr_files(1, &["a"]);
    assert_eq!(trigger(&forge, &db, &pr(1)).await.len(), 1);
    assert_eq!(forge.pr_files_calls(), 1);

    // An unknown files answer: the flag is false and the trigger is decided,
    // not an error.
    let forge = FakeForge::new().with_file(".scarab/ci.yaml", FLAG_ONLY);
    forge.set_pr_files_unknown(1, None);
    let db = Arc::new(InMemoryDb::new());
    assert!(trigger(&forge, &db, &pr(1)).await.is_empty());
    assert_eq!(forge.pr_files_calls(), 1);
}
