//! ADR-0056 amendment — a Rerun of a succeeded step tears down the Pod of any
//! in-flight descendant it supersedes. Driven over the in-memory store so it
//! runs without Postgres. Proves the orphan gap is closed: `rerun_step`
//! re-arms the running descendant AND enqueues a scoped teardown, and
//! `reconcile_supersessions` (which owns the executor) cancels exactly that
//! descendant's Pod — while the run itself stays alive.

use scarab_engine::ports::ExecHandle;
use scarab_engine::{
    cancel_run_request, rerun_step, retry_step, Attempt, AttemptId, AttemptOutcome, Db,
    EventPayload, FailureKind, OutboxId, OutboxMessage, RerunError, RunId, RunStatus, Scheduler,
    StepId, StepSpec, StepStatus, SupersedeTeardown, SupersededAttempt, Timestamp, LAUNCH_STEP,
    MAX_DELIVERY_ATTEMPTS, SUPERSEDE_TEARDOWN,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

fn spec() -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["true".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        clone: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    }
}

/// Rerun `b` (succeeded) while its descendant `c` is in-flight: `c`'s running
/// attempt is superseded, so its Pod is torn down; `c` is re-armed to Pending.
#[tokio::test]
async fn rerun_supersedes_in_flight_descendant_pod() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    let b = StepId("b".into());
    let c = StepId("c".into());
    let a1 = AttemptId("a1".into());
    let handle = "fake://run-1/c/a1";

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    // b has no deps; c depends on b, so invalidating b cascades to c.
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &c,
        Some(&spec()),
        std::slice::from_ref(&b),
        Timestamp(0),
    )
    .await
    .unwrap();

    // b succeeded; c is in-flight (attempt a1 with a launched Pod handle).
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.record_attempt(
        &run,
        &c,
        &Attempt {
            id: a1.clone(),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            output_durability: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    db.set_attempt_handle(&run, &c, &a1, handle).await.unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Running)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Running);

    // The human reruns b.
    let clock = FakeClock::new(1_000);
    rerun_step(&db, &clock, &run, &b, Some("alice".into()))
        .await
        .expect("rerun_step");

    // c was re-armed (its old attempt superseded); b too.
    let steps = db.steps_of_run(&run).await.unwrap();
    let status = |id: &StepId| steps.iter().find(|s| &s.step == id).unwrap().status;
    assert_eq!(status(&c), StepStatus::Pending, "c re-armed");
    assert_eq!(status(&b), StepStatus::Pending, "b re-armed");

    // (a) The abandoned attempt is RECORDED as superseded — the core defect: it
    // previously stayed failure=NULL and served as `failed:false`, rendering the
    // torn-down attempt green.
    let c_attempts = db.attempts_of_step(&run, &c).await.unwrap();
    let a = c_attempts.iter().find(|x| x.id == a1).expect("a1 present");
    assert_eq!(
        a.outcome,
        AttemptOutcome::Superseded,
        "superseded attempt records Superseded, not a silent failed:false"
    );
    assert!(
        a.failure.is_none(),
        "supersession is not a failure classification"
    );
    // ...and the supersession is a durable fact (mirrors AttemptReadopted), with
    // the acting principal attributed.
    let events = db.events(&run).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(&e.kind,
            EventPayload::AttemptSuperseded { step, attempt, by }
                if step == &c && attempt == &a1 && by.as_deref() == Some("alice"))),
        "AttemptSuperseded event appended with attribution"
    );

    // The driver drains the teardown intent and cancels exactly c's Pod.
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched
        .reconcile_supersessions()
        .await
        .expect("reconcile_supersessions");
    assert_eq!(
        exec.cancelled_handles(),
        vec![handle.to_string()],
        "superseded descendant's Pod torn down (and nothing else)"
    );

    // The run itself is untouched — a Rerun forks it forward, never cancels it.
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));
}

/// A Rerun with NO in-flight descendant enqueues no teardown — nothing to cancel.
#[tokio::test]
async fn rerun_without_in_flight_descendant_tears_down_nothing() {
    let db = InMemoryDb::new();
    let run = RunId("run-2".into());
    let b = StepId("b".into());
    let c = StepId("c".into());

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &c,
        Some(&spec()),
        std::slice::from_ref(&b),
        Timestamp(0),
    )
    .await
    .unwrap();
    // Whole run finished: b and c both succeeded (c not in-flight).
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Succeeded);

    let clock = FakeClock::new(1_000);
    rerun_step(&db, &clock, &run, &b, Some("alice".into()))
        .await
        .expect("rerun_step");

    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched.reconcile_supersessions().await.unwrap();
    assert!(
        exec.cancelled_handles().is_empty(),
        "no in-flight descendant ⇒ no teardown"
    );
}

/// ADR-0056 amendment (2026-07-22): a Rerun of a step whose dependency has NOT
/// succeeded is rejected up front (it could only be `dep_dead`-skipped), rather
/// than forking a no-op Take.
#[tokio::test]
async fn rerun_rejects_target_with_unsatisfied_dependency() {
    let db = InMemoryDb::new();
    let run = RunId("run-dep".into());
    let (b, c) = (StepId("b".into()), StepId("c".into()));
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &c,
        Some(&spec()),
        std::slice::from_ref(&b),
        Timestamp(0),
    )
    .await
    .unwrap();
    // b failed; c never ran (dead-dep skipped).
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Failed)
        .await
        .unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Skipped)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Failed);

    let clock = FakeClock::new(1_000);
    let err = rerun_step(&db, &clock, &run, &c, Some("alice".into()))
        .await
        .expect_err("rerun of c must be rejected — b has not succeeded");
    match err {
        RerunError::DependencyNotSatisfied { step, blocker } => {
            assert_eq!(step, c);
            assert_eq!(blocker, b);
        }
        other => panic!("expected DependencyNotSatisfied, got {other:?}"),
    }
    // No Take boundary was forked.
    assert!(
        !db.events(&run)
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, EventPayload::RunRerunRequested { .. })),
        "a rejected rerun must not fork a Take"
    );
}

/// A prerequisite that was SKIPPED (not failed) does NOT block a rerun — only a
/// failed prerequisite does (ADR-0056 amendment). Rerunning such a step is
/// allowed (it forks a Take and re-skips under the all-success join).
#[tokio::test]
async fn rerun_allowed_when_dependency_was_skipped() {
    let db = InMemoryDb::new();
    let run = RunId("run-skip".into());
    let (y, x) = (StepId("y".into()), StepId("x".into()));
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &y, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &x,
        Some(&spec()),
        std::slice::from_ref(&y),
        Timestamp(0),
    )
    .await
    .unwrap();
    // y was skipped (e.g. a `when:` guard); x cascade-skipped behind it.
    db.record_step_transition(&run, &y, StepStatus::Pending, StepStatus::Skipped)
        .await
        .unwrap();
    db.record_step_transition(&run, &x, StepStatus::Pending, StepStatus::Skipped)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Succeeded);

    let clock = FakeClock::new(1_000);
    rerun_step(&db, &clock, &run, &x, Some("alice".into()))
        .await
        .expect("rerun of x is allowed — its dependency was skipped, not failed");
    assert!(
        db.events(&run)
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, EventPayload::RunRerunRequested { .. })),
        "the rerun forked a Take"
    );
}

/// Retry is Failed-only: retrying a Succeeded step is rejected (rerun it instead).
#[tokio::test]
async fn retry_rejects_non_failed_step() {
    let db = InMemoryDb::new();
    let run = RunId("run-retry-ok".into());
    let b = StepId("b".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Succeeded);

    let clock = FakeClock::new(1_000);
    let err = retry_step(&db, &clock, &run, &b, Some("alice".into()))
        .await
        .expect_err("retry of a succeeded step must be rejected");
    assert!(matches!(err, RerunError::NotFailed { status, .. } if status == StepStatus::Succeeded));
}

/// A Retry of a Failed step re-arms it and its dependent cascade **in the current
/// Take**: it emits `StepRetryRequested` (an attribution fact), NOT the
/// Take-boundary `RunRerunRequested`, and reopens the settled run.
#[tokio::test]
async fn retry_reruns_in_take_without_forking() {
    let db = InMemoryDb::new();
    let run = RunId("run-retry".into());
    let (b, c) = (StepId("b".into()), StepId("c".into()));
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &c,
        Some(&spec()),
        std::slice::from_ref(&b),
        Timestamp(0),
    )
    .await
    .unwrap();
    // b failed; c dead-dep skipped; run settled Failed.
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Failed)
        .await
        .unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Skipped)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Failed);

    let clock = FakeClock::new(1_000);
    retry_step(&db, &clock, &run, &b, Some("alice".into()))
        .await
        .expect("retry_step");

    // Cascade re-armed: b (target) and c (dependent) both back to Pending.
    let steps = db.steps_of_run(&run).await.unwrap();
    let status = |id: &StepId| steps.iter().find(|s| &s.step == id).unwrap().status;
    assert_eq!(status(&b), StepStatus::Pending, "b re-armed");
    assert_eq!(status(&c), StepStatus::Pending, "dependent c re-armed");
    // Run reopened so admission picks the re-armed steps back up.
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));

    // The attribution fact is present, but NO Take boundary was forked.
    let events = db.events(&run).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(&e.kind,
            EventPayload::StepRetryRequested { target, by, .. }
                if target == &b && by.as_deref() == Some("alice"))),
        "StepRetryRequested recorded with attribution"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventPayload::RunRerunRequested { .. })),
        "a Retry must NOT fork a Take"
    );
}

/// (b) TOCTOU orphan fix: a descendant that races Ready→Running *between* the
/// rerun's `steps_of_run` snapshot and its guarded re-arm was previously left
/// Running under the old generation with no teardown (a swallowed benign
/// Conflict) — an orphan Pod. It must instead be captured: re-armed from its
/// ACTUAL status, stamped Superseded, and enqueued for teardown.
#[tokio::test]
async fn rerun_captures_descendant_that_raced_into_running() {
    let db = InMemoryDb::new();
    let run = RunId("run-toctou".into());
    let b = StepId("b".into());
    let c = StepId("c".into());
    let a1 = AttemptId("a1".into());

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &c,
        Some(&spec()),
        std::slice::from_ref(&b),
        Timestamp(0),
    )
    .await
    .unwrap();
    // b succeeded; c is Ready (claimed, not yet launched) at snapshot time.
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Ready)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Running);

    // Arm the race: c flips Ready→Running (minting a1) exactly when the rerun's
    // re-arm first tries to move it, so the stale-snapshot guard Conflicts.
    db.arm_toctou_race(&c, &a1);

    let clock = FakeClock::new(1_000);
    rerun_step(&db, &clock, &run, &b, Some("alice".into()))
        .await
        .expect("rerun_step");

    // Despite the race, c ends Pending — re-armed from its ACTUAL Running status.
    let steps = db.steps_of_run(&run).await.unwrap();
    let status = |id: &StepId| steps.iter().find(|s| &s.step == id).unwrap().status;
    assert_eq!(
        status(&c),
        StepStatus::Pending,
        "raced descendant still re-armed"
    );

    // The raced attempt is recorded Superseded (not a silent failed:false)...
    let raced = db
        .attempts_of_step(&run, &c)
        .await
        .unwrap()
        .into_iter()
        .find(|x| x.id == a1)
        .expect("raced attempt a1 recorded");
    assert_eq!(raced.outcome, AttemptOutcome::Superseded);
    assert!(raced.failure.is_none());
    // ...an AttemptSuperseded fact was recorded...
    let events = db.events(&run).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(&e.kind,
            EventPayload::AttemptSuperseded { step, attempt, by }
                if step == &c && attempt == &a1 && by.as_deref() == Some("alice"))),
        "AttemptSuperseded recorded for the raced attempt"
    );
    // ...and it was captured for teardown (the orphan fix): a scoped
    // SUPERSEDE_TEARDOWN intent naming (c, a1) is enqueued.
    let msgs = db
        .claim_outbox("insp", Some(SUPERSEDE_TEARDOWN), 10, 1_000)
        .await
        .unwrap();
    let captured: Vec<(String, String)> = msgs
        .iter()
        .flat_map(|m| {
            serde_json::from_value::<SupersedeTeardown>(m.payload.clone())
                .unwrap()
                .attempts
                .into_iter()
                .map(|x| (x.step, x.attempt))
        })
        .collect();
    assert!(
        captured.contains(&(c.0.clone(), a1.0.clone())),
        "raced descendant captured for teardown, not orphaned: {captured:?}"
    );
}

/// (c) Idempotency-key collision fix: two supersessions in the SAME run at the
/// SAME clock tick previously shared the key `supersede:{run}:{tick}`, so the
/// outbox deduped one teardown away and orphaned its Pod. Distinct supersessions
/// must get distinct keys. Two independent subtrees (`b→c`, `x→y`) reran in one
/// tick each supersede their own in-flight descendant.
#[tokio::test]
async fn same_tick_supersessions_get_distinct_outbox_keys() {
    let db = InMemoryDb::new();
    let run = RunId("run-collide".into());
    let (b, c) = (StepId("b".into()), StepId("c".into()));
    let (x, y) = (StepId("x".into()), StepId("y".into()));
    let (ca1, ya1) = (AttemptId("ca1".into()), AttemptId("ya1".into()));

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    for (s, needs) in [
        (&b, vec![]),
        (&c, vec![b.clone()]),
        (&x, vec![]),
        (&y, vec![x.clone()]),
    ] {
        db.create_step_run(&run, s, Some(&spec()), &needs, Timestamp(0))
            .await
            .unwrap();
    }
    // Both subtrees: parent Succeeded, descendant Running (in-flight).
    for (parent, child, att) in [(&b, &c, &ca1), (&x, &y, &ya1)] {
        db.record_step_transition(&run, parent, StepStatus::Pending, StepStatus::Succeeded)
            .await
            .unwrap();
        db.record_attempt(
            &run,
            child,
            &Attempt {
                id: att.clone(),
                started_at: Timestamp(0),
                failure: None,
                failure_detail: None,
                output_durability: None,
                outcome: AttemptOutcome::Running,
            },
        )
        .await
        .unwrap();
        db.record_step_transition(&run, child, StepStatus::Pending, StepStatus::Running)
            .await
            .unwrap();
    }
    db.seed_run(&run, RunStatus::Running);

    // Never advance the clock: both reruns land in the SAME tick — the exact
    // condition that collided the old key.
    let clock = FakeClock::new(1_000);
    rerun_step(&db, &clock, &run, &b, Some("alice".into()))
        .await
        .expect("rerun b");
    rerun_step(&db, &clock, &run, &x, Some("alice".into()))
        .await
        .expect("rerun x");

    let keys: std::collections::BTreeSet<String> = db
        .claim_outbox("insp", Some(SUPERSEDE_TEARDOWN), 10, 1_000)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.idempotency_key)
        .collect();
    assert_eq!(
        keys.len(),
        2,
        "two same-tick supersessions must get two distinct teardown keys, got {keys:?}"
    );
}

/// Regression (live-caught in real k8s): after a rerun supersedes an in-flight
/// attempt (`a1`) and tears down its Pod, the backend reports that Pod `Lost`.
/// `a1`'s launch intent was never dispatched (it was superseded out of band, not
/// settled), so it lingers on the outbox and `reconcile` re-polls it — observing
/// `Lost`. The engine PREVIOUSLY recorded that `Lost` on `a1`'s attempt row
/// (`set_attempt_failure` ran *before* the "only the frontier attempt may settle"
/// guard), DOWNGRADING the terminal `Superseded` to `{failed:true,
/// failure:"lost", outcome:"failed"}` and rendering the intentionally torn-down
/// attempt as a failure in `AttemptDto`. The immutable event log was always
/// correct (no `AttemptFinished` for `a1`); only the attempts-table
/// denormalization was clobbered. A self-inflicted `Lost` for a non-frontier,
/// already-`Superseded` attempt must be IGNORED — no row write, no event.
#[tokio::test]
async fn superseded_attempt_survives_teardown_induced_lost() {
    let db = InMemoryDb::new();
    let run = RunId("run-lost".into());
    let b = StepId("b".into());
    let c = StepId("c".into());
    let a1 = AttemptId("a1".into());
    let a2 = AttemptId("a2".into());
    let handle = "fake://run-lost/c/a1";

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &c,
        Some(&spec()),
        std::slice::from_ref(&b),
        Timestamp(0),
    )
    .await
    .unwrap();

    // b succeeded; c is in-flight (a1 with a launched Pod handle).
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.record_attempt(
        &run,
        &c,
        &Attempt {
            id: a1.clone(),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            output_durability: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    db.set_attempt_handle(&run, &c, &a1, handle).await.unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Running)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Running);

    // The human reruns b: c's a1 is stamped Superseded and c is re-armed.
    let clock = FakeClock::new(1_000);
    rerun_step(&db, &clock, &run, &b, Some("alice".into()))
        .await
        .expect("rerun_step");

    // c relaunches under a fresh fence — a2 becomes the frontier attempt, so a1
    // is now a non-frontier, superseded generation (the reported live state).
    db.record_attempt(
        &run,
        &c,
        &Attempt {
            id: a2.clone(),
            started_at: Timestamp(1_000),
            failure: None,
            failure_detail: None,
            output_durability: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();

    // a1's launch intent is still on the outbox (never dispatched — a1 never
    // terminally settled). reconcile re-polls it; its SIGTERMed Pod polls `Lost`.
    db.enqueue_outbox(&OutboxMessage {
        id: OutboxId(0),
        run: run.clone(),
        kind: LAUNCH_STEP.to_string(),
        payload: serde_json::json!({ "run": run.0, "step": c.0, "attempt": a1.0 }),
        idempotency_key: format!("launch:{}/{}/{}", run.0, c.0, a1.0),
        at: Timestamp(1_000),
    })
    .await
    .unwrap();

    let exec = FakeExecutor::new();
    exec.kill(ExecHandle(handle.to_string())); // a1's Pod is gone → poll ⇒ Lost
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched
        .reconcile()
        .await
        .expect("reconcile drains the stale a1 launch intent and observes Lost");

    let c_attempts = db.attempts_of_step(&run, &c).await.unwrap();
    let a1_row = c_attempts.iter().find(|x| x.id == a1).expect("a1 present");
    // (a) a1's terminal Superseded is intact — NOT downgraded to failed/lost.
    assert_eq!(
        a1_row.outcome,
        AttemptOutcome::Superseded,
        "the teardown-induced Lost must not downgrade the superseded attempt"
    );
    // (b) supersession is not a failure — `failure` stays None.
    assert!(
        a1_row.failure.is_none(),
        "superseded attempt's `failure` must stay None, not `lost`"
    );

    // (c) no AttemptFinished was appended for a1 — the event log stays correct.
    let events = db.events(&run).await.unwrap();
    assert!(
        !events.iter().any(|e| matches!(&e.kind,
            EventPayload::AttemptFinished { step, attempt, .. }
                if step == &c && attempt == &a1)),
        "no AttemptFinished event for the superseded attempt"
    );

    // (d) the new frontier attempt a2 is unaffected.
    let a2_row = c_attempts.iter().find(|x| x.id == a2).expect("a2 present");
    assert_eq!(
        a2_row.outcome,
        AttemptOutcome::Running,
        "frontier attempt a2 must be untouched"
    );
    assert!(a2_row.failure.is_none(), "frontier a2 carries no failure");
}

/// Regression — the CANCEL-path mirror of
/// `superseded_attempt_survives_teardown_induced_lost`. Cancelling a run stamps
/// its in-flight step `Cancelled` and SIGTERMs that step's Pod (the `CANCEL_RUN`
/// teardown); the dying Pod is then reported `Lost`. `a1`'s launch intent was
/// never dispatched, so it lingers on the outbox and `reconcile` re-polls it —
/// observing `Lost`. Unlike a supersede, a cancel mints NO successor attempt, so
/// the cancelled attempt is STILL the step's frontier — the "only the frontier
/// may settle" guard does NOT catch it; only the recorded `Cancelled` outcome
/// does. The engine PREVIOUSLY guarded only `Superseded`, so
/// `settle_failed_attempt` recorded `failed·lost` on `a1`, DOWNGRADING the
/// terminal `Cancelled` and rendering the intentionally-cancelled attempt as a
/// failure. A self-inflicted `Lost` for an already-`Cancelled` attempt must be
/// IGNORED — no row write, no event.
#[tokio::test]
async fn cancelled_attempt_survives_teardown_induced_lost() {
    let db = InMemoryDb::new();
    let run = RunId("run-cancel-lost".into());
    let b = StepId("b".into());
    let c = StepId("c".into());
    let a1 = AttemptId("a1".into());
    let handle = "fake://run-cancel-lost/c/a1";

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &c,
        Some(&spec()),
        std::slice::from_ref(&b),
        Timestamp(0),
    )
    .await
    .unwrap();

    // b succeeded; c is in-flight (a1 with a launched Pod handle).
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.record_attempt(
        &run,
        &c,
        &Attempt {
            id: a1.clone(),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            output_durability: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    db.set_attempt_handle(&run, &c, &a1, handle).await.unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Running)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Running);

    // The human cancels the run: c → Cancelled, a1 stamped Cancelled, run →
    // Cancelled, and a CANCEL_RUN teardown is enqueued for a1's Pod.
    let clock = FakeClock::new(1_000);
    let cancelled = cancel_run_request(&db, &clock, &run, Some("alice".to_string()))
        .await
        .expect("cancel_run_request");
    assert!(cancelled, "an in-flight run is cancellable");

    // Precondition (the stamp under test): cancel recorded a1 `Cancelled`.
    let a1_stamped = db
        .attempts_of_step(&run, &c)
        .await
        .unwrap()
        .into_iter()
        .find(|x| x.id == a1)
        .expect("a1 present");
    assert_eq!(
        a1_stamped.outcome,
        AttemptOutcome::Cancelled,
        "cancel must stamp the in-flight attempt Cancelled"
    );

    // a1's launch intent is still on the outbox (never dispatched — a1 never
    // terminally settled through the launch path). reconcile re-polls it; its
    // SIGTERMed Pod polls `Lost`.
    db.enqueue_outbox(&OutboxMessage {
        id: OutboxId(0),
        run: run.clone(),
        kind: LAUNCH_STEP.to_string(),
        payload: serde_json::json!({ "run": run.0, "step": c.0, "attempt": a1.0 }),
        idempotency_key: format!("launch:{}/{}/{}", run.0, c.0, a1.0),
        at: Timestamp(1_000),
    })
    .await
    .unwrap();

    let exec = FakeExecutor::new();
    exec.kill(ExecHandle(handle.to_string())); // a1's Pod is gone → poll ⇒ Lost
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched
        .reconcile()
        .await
        .expect("reconcile drains the stale a1 launch intent and observes Lost");

    let c_attempts = db.attempts_of_step(&run, &c).await.unwrap();
    let a1_row = c_attempts.iter().find(|x| x.id == a1).expect("a1 present");
    // (a) a1's terminal Cancelled is intact — NOT downgraded to failed/lost.
    assert_eq!(
        a1_row.outcome,
        AttemptOutcome::Cancelled,
        "the teardown-induced Lost must not downgrade the cancelled attempt"
    );
    // (b) cancellation is not a failure — `failure` stays None.
    assert!(
        a1_row.failure.is_none(),
        "cancelled attempt's `failure` must stay None, not `lost`"
    );

    // (c) it STAYS Cancelled after the Lost — a fresh read of the durable store
    // (not the earlier snapshot) still shows the intentional verdict.
    let a1_persisted = db
        .attempts_of_step(&run, &c)
        .await
        .unwrap()
        .into_iter()
        .find(|x| x.id == a1)
        .expect("a1 present");
    assert_eq!(
        a1_persisted.outcome,
        AttemptOutcome::Cancelled,
        "a1 stays Cancelled after the Lost is drained"
    );
    assert!(a1_persisted.failure.is_none());

    // (d) no AttemptFinished was appended for a1 — the event log stays correct.
    let events = db.events(&run).await.unwrap();
    assert!(
        !events.iter().any(|e| matches!(&e.kind,
            EventPayload::AttemptFinished { step, attempt, .. }
                if step == &c && attempt == &a1)),
        "no spurious AttemptFinished event for the cancelled attempt"
    );
}

/// Attempts that share a `started_at` (common under `FakeClock`, possible under
/// fast real execution) must still order by mint sequence, so `.last()` — the
/// frontier that anchors `?attempt=` reads and the settle-path frontier guard —
/// is deterministic. Without the numeric-suffix tiebreak this is
/// nondeterministic; a lexical tiebreak would wrongly place `a10` before `a2`.
/// Exercises the in-memory `Db`, which mirrors the postgres `ORDER BY`.
#[tokio::test]
async fn attempts_order_by_mint_sequence_on_started_at_tie() {
    let db = InMemoryDb::new();
    let run = RunId("run-order".into());
    let step = StepId("s".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &step, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();

    // a1 and a2 minted with the SAME started_at, inserted a2-then-a1 to prove
    // the order comes from the id's mint sequence, not insertion order.
    let running = |id: &str| Attempt {
        id: AttemptId(id.into()),
        started_at: Timestamp(7),
        failure: None,
        failure_detail: None,
        output_durability: None,
        outcome: AttemptOutcome::Running,
    };
    db.record_attempt(&run, &step, &running("a2"))
        .await
        .unwrap();
    db.record_attempt(&run, &step, &running("a1"))
        .await
        .unwrap();

    let attempts = db.attempts_of_step(&run, &step).await.unwrap();
    let ids: Vec<&str> = attempts.iter().map(|a| a.id.0.as_str()).collect();
    assert_eq!(
        ids,
        ["a1", "a2"],
        "equal started_at ties break on mint order"
    );
    assert_eq!(
        attempts.last().unwrap().id,
        AttemptId("a2".into()),
        "frontier = latest-minted attempt (a2)"
    );

    // Lexical trap: with the same started_at, `a10` must sort AFTER `a2`
    // (numeric suffix), not before it (lexical string order).
    db.record_attempt(&run, &step, &running("a10"))
        .await
        .unwrap();
    let attempts = db.attempts_of_step(&run, &step).await.unwrap();
    let ids: Vec<&str> = attempts.iter().map(|a| a.id.0.as_str()).collect();
    assert_eq!(
        ids,
        ["a1", "a2", "a10"],
        "tiebreak is numeric, not lexical (a10 last)"
    );
    assert_eq!(attempts.last().unwrap().id, AttemptId("a10".into()));

    // steps_of_run must expose the same frontier as attempts_of_step.
    let steps = db.steps_of_run(&run).await.unwrap();
    let s_attempts = &steps.iter().find(|s| s.step == step).unwrap().attempts;
    assert_eq!(
        s_attempts.last().unwrap().id,
        AttemptId("a10".into()),
        "steps_of_run frontier matches attempts_of_step"
    );
}

/// `record_attempt` mints a FRESH Running row at launch/adoption; if it is ever
/// re-invoked for an id whose evidence was already recorded (crash re-adoption,
/// idempotent re-launch), it must NEVER downgrade that evidence back to
/// running/NULL. Mirror of the postgres `ON CONFLICT ... DO NOTHING`.
#[tokio::test]
async fn record_attempt_never_downgrades_recorded_evidence() {
    let db = InMemoryDb::new();
    let run = RunId("run-nodown".into());
    let step = StepId("s".into());
    let a1 = AttemptId("a1".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &step, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();

    // Launch mints the fresh Running row.
    db.record_attempt(
        &run,
        &step,
        &Attempt {
            id: a1.clone(),
            started_at: Timestamp(5),
            failure: None,
            failure_detail: None,
            output_durability: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();

    // Evidence recorded: the attempt failed (failure + Failed outcome move
    // together — ADR-0056 amendment).
    db.set_attempt_failure(&run, &step, &a1, FailureKind::Step, None)
        .await
        .unwrap();

    // Re-drive: record_attempt fires again for a1 with a fresh Running row (and
    // a different started_at, to prove nothing on the row is clobbered).
    db.record_attempt(
        &run,
        &step,
        &Attempt {
            id: a1.clone(),
            started_at: Timestamp(999),
            failure: None,
            failure_detail: None,
            output_durability: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();

    let attempts = db.attempts_of_step(&run, &step).await.unwrap();
    assert_eq!(
        attempts.len(),
        1,
        "re-record is idempotent — no duplicate row"
    );
    let a = &attempts[0];
    assert_eq!(
        a.outcome,
        AttemptOutcome::Failed,
        "recorded outcome NOT downgraded to Running"
    );
    assert_eq!(
        a.failure,
        Some(FailureKind::Step),
        "recorded failure NOT reset to NULL"
    );
    assert_eq!(
        a.started_at,
        Timestamp(5),
        "original started_at preserved (budget source)"
    );
}

// ---------------------------------------------------------------------------
// Teardown reliability (git-bug fd6e6d4): a failed `cancel` in the supersede
// teardown drainer must retry (bounded by the ADR-0047 poison ceiling) instead
// of silently retiring the outbox message and orphaning the Pod.
// ---------------------------------------------------------------------------

/// Seed a run with one in-flight step whose Pod handle is recorded, and enqueue
/// a `SUPERSEDE_TEARDOWN` naming that attempt — the fixture for the teardown
/// retry / dead-letter tests. Returns the run id and the Pod handle.
async fn seed_supersede_teardown(db: &InMemoryDb, run_name: &str) -> (RunId, String) {
    let run = RunId(run_name.into());
    let c = StepId("c".into());
    let a1 = AttemptId("a1".into());
    let handle = format!("fake://{run_name}/c/a1");

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &c, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.record_attempt(
        &run,
        &c,
        &Attempt {
            id: a1.clone(),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            output_durability: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    db.set_attempt_handle(&run, &c, &a1, &handle).await.unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Running)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Running);

    db.enqueue_outbox(&OutboxMessage {
        id: OutboxId(0),
        run: run.clone(),
        kind: SUPERSEDE_TEARDOWN.to_string(),
        payload: serde_json::to_value(SupersedeTeardown {
            attempts: vec![SupersededAttempt {
                step: c.0.clone(),
                attempt: a1.0.clone(),
            }],
        })
        .unwrap(),
        idempotency_key: format!("supersede:{run_name}"),
        at: Timestamp(0),
    })
    .await
    .unwrap();

    (run, handle)
}

/// A cancel that genuinely fails (a transient k8s API error — NOT an
/// already-gone Pod, which the adapters fold into `Ok`) must NOT retire the
/// teardown message: doing so silently orphans the Pod. The message stays
/// claimable and a later reconcile — once the cancel succeeds — retires it, so
/// the Pod is torn down rather than leaked.
#[tokio::test]
async fn supersede_teardown_retries_when_cancel_fails() {
    let db = InMemoryDb::new();
    let (_run, handle) = seed_supersede_teardown(&db, "run-retry").await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.fail_cancels(1); // the first cancel errors; the next succeeds
                          // visibility 0 ⇒ an un-dispatched (still-claimed) message is re-servable on
                          // the next reconcile, modelling the claim-lease lapse a production tick
                          // waits out before retrying.
    let sched = Scheduler::new(&db, &clock, &exec, "drv").with_outbox_visibility_ms(0);

    // First pass: the cancel fails, so the message is NOT retired.
    sched
        .reconcile_supersessions()
        .await
        .expect("first reconcile");
    assert_eq!(
        exec.cancelled_handles(),
        vec![handle.clone()],
        "cancel was attempted once"
    );
    assert_eq!(
        db.outbox_depth().await.unwrap(),
        1,
        "a failed cancel must not retire the teardown — the Pod would be orphaned"
    );

    // Second pass: the cancel now succeeds, so the message is retired.
    sched
        .reconcile_supersessions()
        .await
        .expect("second reconcile");
    assert_eq!(
        exec.cancelled_handles(),
        vec![handle.clone(), handle.clone()],
        "the teardown was retried (cancel attempted a second time)"
    );
    assert_eq!(
        db.outbox_depth().await.unwrap(),
        0,
        "a successful retry retires the teardown"
    );
}

/// A cancel that fails persistently (a Pod the backend can never reach) must
/// not spin forever: the teardown rides the same delivery-attempt/poison bound
/// as launch intents (ADR-0047) and dead-letters after `MAX_DELIVERY_ATTEMPTS`,
/// recording a diagnostic — without ever touching the run's own state.
#[tokio::test]
async fn supersede_teardown_dead_letters_after_max_delivery_attempts() {
    let db = InMemoryDb::new();
    let (run, _handle) = seed_supersede_teardown(&db, "run-poison").await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.fail_cancels(u32::MAX); // a Pod the backend can never tear down
    let sched = Scheduler::new(&db, &clock, &exec, "drv").with_outbox_visibility_ms(0);

    // Drive one reconcile per delivery until the poison bound trips.
    for _ in 0..MAX_DELIVERY_ATTEMPTS {
        sched.reconcile_supersessions().await.expect("reconcile");
    }

    // It was retried up to the bound, then abandoned.
    assert_eq!(
        exec.cancelled_handles().len() as u32,
        MAX_DELIVERY_ATTEMPTS,
        "teardown retried exactly up to the delivery bound"
    );
    assert_eq!(
        db.outbox_depth().await.unwrap(),
        0,
        "the poison teardown is dead-lettered (dropped from the backlog gauge)"
    );

    // Dead-lettered ⇒ never claimed again (a further reconcile is a no-op).
    sched
        .reconcile_supersessions()
        .await
        .expect("post-dead-letter reconcile");
    assert_eq!(
        exec.cancelled_handles().len() as u32,
        MAX_DELIVERY_ATTEMPTS,
        "a dead-lettered teardown is never redelivered"
    );

    // A diagnostic was recorded on the event log — the operator signal (the
    // failure is no longer silent).
    let events = db.events(&run).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(&e.kind,
            EventPayload::Raw(v)
                if v.get("event").and_then(|x| x.as_str()) == Some("TeardownAbandoned"))),
        "a TeardownAbandoned diagnostic is appended when teardown is abandoned"
    );

    // The run itself is untouched — teardown is pure resource hygiene.
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Running),
        "an abandoned teardown never changes the run's own state"
    );
}

/// The desired end state is "no live Pod", so a cancel that returns `Ok` —
/// whether it tore the Pod down or found it already gone (the adapters fold a
/// missing Pod into `Ok`) — retires the teardown on the FIRST pass, with no
/// spurious retry.
#[tokio::test]
async fn supersede_teardown_ok_cancel_retires_on_first_pass() {
    let db = InMemoryDb::new();
    let (_run, handle) = seed_supersede_teardown(&db, "run-gone").await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new(); // cancel returns Ok (cancelled-or-already-gone)
    let sched = Scheduler::new(&db, &clock, &exec, "drv").with_outbox_visibility_ms(0);

    sched.reconcile_supersessions().await.expect("reconcile");
    assert_eq!(
        db.outbox_depth().await.unwrap(),
        0,
        "an Ok cancel (cancelled or already-gone) retires the teardown at once"
    );

    // A second reconcile is a no-op — the dispatched message is never re-served,
    // so there is no spurious re-cancel.
    sched
        .reconcile_supersessions()
        .await
        .expect("second reconcile");
    assert_eq!(
        exec.cancelled_handles(),
        vec![handle],
        "no spurious retry: cancel happened exactly once"
    );
}
