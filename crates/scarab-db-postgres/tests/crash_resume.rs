//! ACCEPTANCE — the durability wedge (ADR-0017 wedge exception, 0002, 0022).
//!
//! Kill the control plane mid-run and prove the step is executed effectively
//! ONCE across the crash: the run resumes from durable Postgres state and
//! completes, with exactly one attempt, one launch fence, and one recorded
//! terminal transition — no duplicate execution, no double completion.
//!
//! Faithful to ADR-0017's "kill the engine mid-DAG against real Postgres": the
//! durable store is real Postgres; the *executor* (the true external — a k8s
//! Pod) is a fake that OUTLIVES the control-plane process, so "crash the
//! scheduler" == drop instance A and rebuild instance B over the same Postgres
//! and the same still-running Pod. (The live kind variant runs from the dev
//! harness; it is unavailable in this environment.)
//!
//! Skips cleanly when SCARAB_TEST_DATABASE_URL is unset (see `common`).

mod common;

use std::time::Duration;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    Attempt, AttemptId, Db, EventPayload, RunId, RunStatus, Scheduler, StepId, StepRun, StepSpec,
    StepStatus, Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor};

#[tokio::test]
async fn crash_mid_run_resumes_and_runs_step_exactly_once() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-crash".into());
    let step = StepId("deploy".into());
    let spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec!["sh".into(), "-c".into(), "sleep 3; echo done".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
    };
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &step, Some(&spec), &[], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    // The Pod: shared across both control-plane instances (it outlives the crash).
    // No scripted outcome yet, so poll() reports Running — the step is in flight.
    let exec = FakeExecutor::new();

    // The deterministic launch fence/handle for this step's first attempt.
    let fenced = StepRun {
        run: run.clone(),
        step: step.clone(),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
        }],
        needs: vec![],
        gate_kind: None,
    };
    let handle = FakeExecutor::handle_for(&fenced);

    // --- Control-plane instance A: admit + launch, then it is polling a still-
    // running Pod when the process is killed. ---
    {
        let sched_a = Scheduler::new(&db, &clock, &exec, "scarab-1").with_outbox_visibility_ms(200);
        sched_a.tick(&run).await.unwrap();
    }
    // Mid-run snapshot: one attempt, launched once, nothing terminal yet.
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Running);
    assert_eq!(steps[0].attempts.len(), 1, "one attempt launched");
    assert_eq!(exec.launch_count(&handle), 1, "Pod launched once");

    // --- CRASH: instance A is gone. The Pod finishes during the outage. ---
    exec.script_outcome(ExecState::Succeeded);
    // Let the outbox claim-lease expire so the resumed process can reclaim the
    // in-flight launch intent (models wall-clock passing during the restart).
    tokio::time::sleep(Duration::from_millis(300)).await;

    // --- Restart: instance B over the SAME Postgres and the SAME Pod. ---
    {
        let sched_b = Scheduler::new(&db, &clock, &exec, "scarab-1").with_outbox_visibility_ms(200);
        for _ in 0..5 {
            sched_b.tick(&run).await.unwrap();
            if db.run_status(&run).await.unwrap() == Some(RunStatus::Succeeded) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // --- Exactly-once assertions. ---
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded),
        "run resumed and completed"
    );
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Succeeded);
    assert_eq!(
        steps[0].attempts.len(),
        1,
        "NO second attempt minted across the crash — one execution unit"
    );

    let events = db.events(&run).await.unwrap();
    let count = |pred: &dyn Fn(&EventPayload) -> bool| events.iter().filter(|e| pred(&e.kind)).count();
    assert_eq!(
        count(&|k| matches!(k, EventPayload::AttemptStarted { .. })),
        1,
        "step started exactly once"
    );
    assert_eq!(
        count(&|k| matches!(k, EventPayload::AttemptFinished { .. })),
        1,
        "step finished (exit recorded) exactly once — no double completion"
    );
    assert_eq!(
        count(&|k| matches!(
            k,
            EventPayload::StepTransitioned { to: StepStatus::Succeeded, .. }
        )),
        1,
        "one terminal transition"
    );

    // The resumed process ADOPTED the still-running Pod via the durable launch
    // handle recorded by instance A (ADR-0047 re-adoption): same Attempt, same
    // fence, supervision resumed — launch was never even re-called, so the
    // external effect happened exactly once.
    assert_eq!(exec.launch_count(&handle), 1, "adopted — never relaunched");

    tdb.cleanup().await;
}
