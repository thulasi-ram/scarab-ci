//! Post-checks-back acceptance (ADR-0010, 0013): as a run transitions, commit
//! statuses are posted to the forge via the outbox — start → pending,
//! success → success, failure → failure — and redraining is idempotent. The
//! forge HTTP boundary is a FakeForge; the store is InMemoryDb (no network).

use std::sync::Arc;

use scarab_engine::ports::ExecState;
use scarab_engine::{Db, RunId, RunStatus, Scheduler, StepStatus};
use scarab_forge::{Event, RepoRef, StatusState};
use scarab_server::{drain_forge_statuses, trigger_run_from_event};
use scarab_testkit::{FakeClock, FakeExecutor, FakeForge, InMemoryDb};

const CI_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        repo: RepoRef {
            owner: "acme".into(),
            name: "app".into(),
        },
        r#ref: "refs/heads/main".into(),
        after: "sha123".into(),
        message: "build the thing".into(),
    }
}

/// Drive the run to terminal with a scripted executor outcome (bounded).
async fn drive(db: &InMemoryDb, clock: &FakeClock, exec: &FakeExecutor, run: &RunId) {
    let sched = Scheduler::new(db, clock, exec, "sched-1");
    for _ in 0..10 {
        sched.tick(run).await.expect("tick");
        if db.run_status(run).await.unwrap().unwrap().is_terminal() {
            return;
        }
    }
    panic!("run did not settle");
}

#[tokio::test]
async fn run_start_and_success_post_pending_then_success() {
    let forge = Arc::new(FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML));
    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);

    // Trigger a run from a push; then drive it to success.
    let run = trigger_run_from_event(forge.as_ref(), &db, &clock, None, &push())
        .await
        .unwrap()
        .pop()
        .expect("push starts a run");
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);
    drive(&db, &clock, &exec, &run).await;
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );

    // Drain status notifications to the forge: Running -> pending, then
    // Succeeded -> success, in order.
    let posted = drain_forge_statuses(
        forge.as_ref(),
        &db,
        "drainer",
        32,
        30_000,
        "http://scarab.test",
    )
    .await
    .unwrap();
    assert_eq!(posted, 2);
    let states: Vec<StatusState> = forge.statuses().iter().map(|s| s.state).collect();
    assert_eq!(states, vec![StatusState::Pending, StatusState::Success]);
    assert!(forge.statuses().iter().all(|s| s.context == "scarab"));
    // Every status carries the REQUIRED run deep-link (ADR-0046).
    assert!(
        forge
            .statuses()
            .iter()
            .all(|s| s.target_url == format!("http://scarab.test/runs/{}", run.0)),
        "statuses deep-link to the run: {:?}",
        forge.statuses()
    );

    // Redraining is a no-op — dispatched messages are not re-posted (idempotent).
    let again = drain_forge_statuses(
        forge.as_ref(),
        &db,
        "drainer",
        32,
        30_000,
        "http://scarab.test",
    )
    .await
    .unwrap();
    assert_eq!(again, 0);
    assert_eq!(forge.statuses().len(), 2, "no duplicate posts");
}

#[tokio::test]
async fn run_failure_posts_failure_status() {
    let forge = Arc::new(FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML));
    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);

    let run = trigger_run_from_event(forge.as_ref(), &db, &clock, None, &push())
        .await
        .unwrap()
        .pop()
        .expect("push starts a run");
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Failed {
        exit_code: Some(1),
        class: scarab_engine::ports::FailureClass::Step,
    });
    drive(&db, &clock, &exec, &run).await;
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Failed));
    // The build step actually failed (sanity).
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Failed);

    drain_forge_statuses(
        forge.as_ref(),
        &db,
        "drainer",
        32,
        30_000,
        "http://scarab.test",
    )
    .await
    .unwrap();
    let states: Vec<StatusState> = forge.statuses().iter().map(|s| s.state).collect();
    assert_eq!(states, vec![StatusState::Pending, StatusState::Failure]);
}

#[tokio::test]
async fn rejected_status_posts_are_retried_then_dead_lettered_not_silently_dropped() {
    // ba921db: a forge that rejects the post (e.g. an App missing statuses:write
    // → HTTP 403 "Resource not accessible by integration") must not loop forever
    // silently. The message is retried, then dead-lettered as poison — so the
    // outbox stops churning and the failure is observable, while the run itself
    // is unaffected (it already succeeded).
    let forge = Arc::new(
        FakeForge::new()
            .with_file(".scarab/ci.yaml", CI_YAML)
            .failing_status(),
    );
    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);

    let run = trigger_run_from_event(forge.as_ref(), &db, &clock, None, &push())
        .await
        .unwrap()
        .pop()
        .expect("push starts a run");
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);
    drive(&db, &clock, &exec, &run).await;
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );

    // Counters are process-global, so measure the DELTA this drain causes.
    let failures_before = scarab_server::metrics::forge_status_failures();
    let dead_before = scarab_server::metrics::forge_status_dead_lettered();

    // visibility_ms = 0 makes each claimed-but-unposted message immediately
    // reclaimable, so repeated drains accumulate failures on the same messages.
    for _ in 0..scarab_engine::MAX_DELIVERY_ATTEMPTS {
        let posted =
            drain_forge_statuses(forge.as_ref(), &db, "drainer", 32, 0, "http://scarab.test")
                .await
                .unwrap();
        assert_eq!(posted, 0, "a rejected post is never counted as posted");
    }
    assert!(
        forge.statuses().is_empty(),
        "no status ever landed on the forge"
    );

    // After MAX_DELIVERY_ATTEMPTS failed deliveries the status messages are
    // dead-lettered (poison), so they drop out of the outbox and a further drain
    // claims nothing — the loop is broken instead of retrying forever.
    let after = drain_forge_statuses(forge.as_ref(), &db, "drainer", 32, 0, "http://scarab.test")
        .await
        .unwrap();
    assert_eq!(after, 0);
    assert_eq!(
        db.outbox_depth().await.unwrap(),
        0,
        "dead-lettered status messages no longer sit in the outbox"
    );

    // …and every rejection is COUNTED, so a broken App is visible on /metrics
    // instead of only in a log line nobody is grepping (ba921db). Two messages
    // (pending, success) × MAX_DELIVERY_ATTEMPTS rejections each, then both
    // retired as poison.
    let attempts = u64::from(scarab_engine::MAX_DELIVERY_ATTEMPTS);
    assert_eq!(
        scarab_server::metrics::forge_status_failures() - failures_before,
        2 * attempts,
        "each rejected post increments the failure counter"
    );
    assert_eq!(
        scarab_server::metrics::forge_status_dead_lettered() - dead_before,
        2,
        "each poisoned status message increments the dead-letter counter"
    );
}
