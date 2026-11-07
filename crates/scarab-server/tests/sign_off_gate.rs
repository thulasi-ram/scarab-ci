//! A sign-off gate whose only dependent `when:` excluded (git-bug 0130969).
//! 2c37802 skips a waiting gate whose dependents are all terminal "so the
//! Run settles Failed at once". A `when:` exclusion is not a failure: the
//! gate is a sign-off on `build`, so it is reached and waits for someone,
//! and the Run does not settle Succeeded with nobody asked.

use std::sync::Arc;

use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, EventPayload, RunId, RunStatus, Scheduler, StepStatus};
use scarab_forge::RepoRef;
use scarab_server::{dispatch_run, DispatchKind};
use scarab_testkit::{FakeClock, FakeExecutor, FakeForge, InMemoryDb};

const SIGN_OFF: &str = r#"
on:
  manual: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: signoff, gate: manual, needs: [build] }
  - { id: publish, image: busybox, command: ["true"], needs: [signoff], when: "event.kind == 'push'" }
"#;

async fn status(db: &InMemoryDb, run: &RunId, id: &str) -> StepStatus {
    db.steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == id)
        .unwrap()
        .status
}

#[tokio::test]
async fn a_sign_off_gate_whose_dependents_when_excluded_is_asked_not_skipped() {
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let run = dispatch_run(
        &FakeForge::new()
            .with_file(".scarab/release.yaml", SIGN_OFF)
            .with_commit(
                "refs/heads/main",
                "1234567890abcdef1234567890abcdef12345678",
            ),
        db.as_ref(),
        clock.as_ref(),
        scarab_server::AdmissionPorts::governance(None),
        "bob".into(),
        RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        "refs/heads/main".into(),
        "release".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect("admitted");
    assert_eq!(status(&db, &run, "publish").await, StepStatus::Skipped);

    let exec = FakeExecutor::new();
    let sched = Scheduler::new(db.as_ref(), clock.as_ref() as &dyn Clock, &exec, "sched");
    for _ in 0..6 {
        exec.script_outcome(ExecState::Succeeded);
        sched.tick(&run).await.unwrap();
    }

    assert_eq!(status(&db, &run, "build").await, StepStatus::Succeeded);
    assert_eq!(
        status(&db, &run, "signoff").await,
        StepStatus::Pending,
        "the sign-off waits for someone"
    );
    let skipped_unneeded = db.events(&run).await.unwrap().iter().any(|e| {
        matches!(&e.kind, EventPayload::StepSkipped { step, reason }
            if step.0 == "signoff" && reason == scarab_engine::GATE_NO_LONGER_NEEDED)
    });
    assert!(!skipped_unneeded, "not skipped as no longer needed");
    let run_status = db.run_status(&run).await.unwrap();
    assert!(
        !run_status.is_some_and(RunStatus::is_terminal),
        "the Run has not settled: {run_status:?}"
    );
}
