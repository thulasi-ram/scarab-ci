//! `Db::deployment_requesting_runs` (slice b, criterion 4): how many Runs —
//! other than the asking one — are **in flight** on a DeploymentTarget, which
//! is what the per-target concurrency hold counts.
//!
//! In flight = the Run is not terminal and a Step of it requesting the target
//! is `Ready`/`Running`, or `Succeeded` with a DeploymentGate on that request
//! not yet terminal — reached (every need of it Succeeded), or not reached
//! and its requesting Step's latest Attempt start after the caller's cutoff. Distinct Runs,
//! scoped to (org, environment, source, id). Also in flight, whatever the
//! Run's status: a requester whose unit's teardown (a cancel or a rerun's
//! supersession) is not yet retired (git-bug f22056d).
//! One scenario, run against real Postgres (skipped when
//! `SCARAB_TEST_DATABASE_URL` is unset) and the in-memory twin, so the two
//! stay equivalent.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Db, RunDeploymentRequest, RunId, RunStatus, StepId,
    StepSpec, StepStatus, Timestamp,
};
use scarab_testkit::InMemoryDb;

const ORG: &str = "acme";

fn spec() -> StepSpec {
    serde_json::from_value(serde_json::json!({
        "image": "busybox:latest",
        "command": ["true"],
        "env": [],
    }))
    .unwrap()
}

/// The guarded transitions that walk a fresh `Pending` step to `to`.
fn path(to: StepStatus) -> &'static [(StepStatus, StepStatus)] {
    use StepStatus::*;
    match to {
        Pending => &[],
        Ready => &[(Pending, Ready)],
        Running => &[(Pending, Ready), (Ready, Running)],
        Succeeded => &[(Pending, Ready), (Ready, Running), (Running, Succeeded)],
        Failed => &[(Pending, Ready), (Ready, Running), (Running, Failed)],
        other => panic!("no path to {other:?} in this test"),
    }
}

/// One requesting step: `(step, environment, target id, status, gate_steps)`.
type Requester<'a> = (&'a str, &'a str, &'a str, StepStatus, &'a [&'a str]);

/// Seed Run `run` (left `run_status`) with its requesting steps and its
/// gates `(gate, status)`, the gates needing nothing (only their status is
/// read here).
async fn seed(
    db: &dyn Db,
    run: &str,
    run_status: RunStatus,
    requesters: &[Requester<'_>],
    gates: &[(&str, StepStatus)],
) {
    let run = RunId(run.into());
    db.create_run(&run, 2, 1, Timestamp(1)).await.unwrap();
    let mut requests = Vec::new();
    for (ordinal, (step, env, target, status, gate_steps)) in requesters.iter().enumerate() {
        let step = StepId((*step).into());
        db.create_step_run(&run, &step, Some(&spec()), &[], Timestamp(1))
            .await
            .unwrap();
        for (from, to) in path(*status) {
            db.record_step_transition(&run, &step, *from, *to)
                .await
                .unwrap();
        }
        requests.push(RunDeploymentRequest {
            step,
            ordinal: ordinal as u32,
            org: ORG.into(),
            environment: (*env).into(),
            target_source: "argocd".into(),
            target_id: (*target).into(),
            version_result: "version".into(),
            evidence_results: Vec::new(),
            gate_steps: gate_steps.iter().map(|g| StepId((*g).into())).collect(),
        });
    }
    for (gate, status) in gates {
        let gate = StepId((*gate).into());
        db.create_step_run(&run, &gate, None, &[], Timestamp(1))
            .await
            .unwrap();
        db.set_step_gate(&run, &gate, "deployment", None)
            .await
            .unwrap();
        if *status != StepStatus::Pending {
            db.record_step_transition(&run, &gate, StepStatus::Pending, *status)
                .await
                .unwrap();
        }
    }
    db.set_run_deployment_requests(&run, &requests)
        .await
        .unwrap();
    match run_status {
        RunStatus::Pending => {}
        RunStatus::Running => db
            .record_transition(&run, RunStatus::Pending, RunStatus::Running)
            .await
            .unwrap(),
        RunStatus::Suspended => {
            db.record_transition(&run, RunStatus::Pending, RunStatus::Running)
                .await
                .unwrap();
            db.record_transition(&run, RunStatus::Running, RunStatus::Suspended)
                .await
                .unwrap();
        }
        terminal => {
            db.record_transition(&run, RunStatus::Pending, RunStatus::Running)
                .await
                .unwrap();
            db.record_transition(&run, RunStatus::Running, terminal)
                .await
                .unwrap();
        }
    }
}

/// Every gate [`seed`] makes needs nothing, so it is **reached**: the
/// unreached-gate cutoff is passed as late as it goes, so a Run here counts
/// through a reached gate alone (see
/// [`an_unreached_gate_stops_counting_at_the_cutoff`] for the cutoff).
async fn count(db: &dyn Db, env: &str, target: &str, excluding: &str) -> u32 {
    count_at(db, env, target, excluding, Timestamp(i64::MAX)).await
}

async fn count_at(
    db: &dyn Db,
    env: &str,
    target: &str,
    excluding: &str,
    unreached_gate_cutoff: Timestamp,
) -> u32 {
    db.deployment_requesting_runs(
        ORG,
        env,
        "argocd",
        target,
        &RunId(excluding.into()),
        unreached_gate_cutoff,
    )
    .await
    .unwrap()
}

async fn in_flight_runs_are_counted_once_each(db: &dyn Db) {
    use RunStatus as R;
    use StepStatus as S;
    // Not in flight: still Pending (held or not reached), failed (made no
    // request), succeeded with its gate's verdict in, succeeded with no gate.
    seed(
        db,
        "x-pending",
        R::Running,
        &[("bump", "prod", "web", S::Pending, &["live"])],
        &[("live", S::Pending)],
    )
    .await;
    seed(
        db,
        "x-failed",
        R::Running,
        &[("bump", "prod", "web", S::Failed, &["live"])],
        &[("live", S::Pending)],
    )
    .await;
    seed(
        db,
        "x-verdict",
        R::Running,
        &[("bump", "prod", "web", S::Succeeded, &["live"])],
        &[("live", S::Succeeded)],
    )
    .await;
    seed(
        db,
        "x-gate-failed",
        R::Running,
        &[("bump", "prod", "web", S::Succeeded, &["live"])],
        &[("live", S::Failed)],
    )
    .await;
    seed(
        db,
        "x-nogate",
        R::Running,
        &[("bump", "prod", "web", S::Succeeded, &[])],
        &[],
    )
    .await;
    // A terminal Run is never in flight, whatever its rows still say.
    seed(
        db,
        "x-cancelled",
        R::Cancelled,
        &[("bump", "prod", "web", S::Running, &[])],
        &[],
    )
    .await;
    // Elsewhere: another target, another Environment.
    seed(
        db,
        "x-api",
        R::Running,
        &[("bump", "prod", "api", S::Running, &[])],
        &[],
    )
    .await;
    seed(
        db,
        "x-staging",
        R::Running,
        &[("bump", "staging", "web", S::Running, &[])],
        &[],
    )
    .await;
    assert_eq!(
        count(db, "prod", "web", "nobody").await,
        0,
        "nothing in flight on prod/web yet"
    );

    // In flight: promoted, running, or succeeded with its gate's verdict
    // outstanding (in a Suspended Run, too).
    seed(
        db,
        "x-ready",
        R::Running,
        &[("bump", "prod", "web", S::Ready, &[])],
        &[],
    )
    .await;
    seed(
        db,
        "x-running",
        R::Running,
        &[("bump", "prod", "web", S::Running, &["live"])],
        &[("live", S::Pending)],
    )
    .await;
    seed(
        db,
        "x-gated",
        R::Suspended,
        &[("bump", "prod", "web", S::Succeeded, &["live"])],
        &[("live", S::Pending)],
    )
    .await;
    // Two of one Run's Steps in flight on the target: ONE Run.
    seed(
        db,
        "x-two",
        R::Running,
        &[
            ("bump1", "prod", "web", S::Running, &[]),
            ("bump2", "prod", "web", S::Ready, &[]),
        ],
        &[],
    )
    .await;

    assert_eq!(count(db, "prod", "web", "nobody").await, 4);
    assert_eq!(
        count(db, "prod", "web", "x-running").await,
        3,
        "the asking Run is excluded"
    );
    assert_eq!(
        count(db, "prod", "web", "x-pending").await,
        4,
        "excluding a Run not in flight changes nothing"
    );
    assert_eq!(count(db, "prod", "api", "nobody").await, 1);
    assert_eq!(count(db, "staging", "web", "nobody").await, 1);
    assert_eq!(count(db, "prod", "nope", "nobody").await, 0);
}

#[tokio::test]
async fn pg_in_flight_runs_are_counted_once_each() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    in_flight_runs_are_counted_once_each(&db).await;
    tdb.cleanup().await;
}

#[tokio::test]
async fn inmemory_in_flight_runs_are_counted_once_each() {
    in_flight_runs_are_counted_once_each(&InMemoryDb::new()).await;
}

/// A launched requester `bump` (prod/web) in Run `run`: `Running` on `a1`,
/// whose Pod handle is recorded.
async fn seed_launched(db: &dyn Db, run: &str) {
    seed(
        db,
        run,
        RunStatus::Running,
        &[("bump", "prod", "web", StepStatus::Running, &[])],
        &[],
    )
    .await;
    let (run, bump, a1) = (
        RunId(run.into()),
        StepId("bump".into()),
        AttemptId("a1".into()),
    );
    db.record_attempt(
        &run,
        &bump,
        &Attempt {
            id: a1.clone(),
            started_at: Timestamp(2),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    db.set_attempt_handle(&run, &bump, &a1, &format!("pod-{}", run.0))
        .await
        .unwrap();
}

/// The one pending teardown message of `kind` for `run`.
async fn teardown_of(db: &dyn Db, run: &str, kind: &str) -> scarab_engine::OutboxId {
    let mine: Vec<_> = db
        .claim_outbox("test", Some(kind), 100, 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.run.0 == run)
        .collect();
    assert_eq!(mine.len(), 1, "one {kind} intent for {run}: {mine:?}");
    mine[0].id
}

/// A cancelled or superseded requester's deploy Step stops being in flight
/// at once, but its unit runs on through its termination grace, still acting
/// on the target (git-bug f22056d). So it counts — whatever its Run's status
/// — until its teardown intent is retired: dispatched (the unit is gone) or
/// dead-lettered (abandoned, with a diagnostic). A cancel that caught the
/// Step before anything launched holds nothing.
async fn a_requester_counts_until_its_teardown_is_retired(db: &dyn Db) {
    use scarab_engine::{cancel_run_request, rerun_step, CANCEL_ATTEMPT, SUPERSEDE_TEARDOWN};
    let clock = scarab_testkit::FakeClock::new(10);
    let web = || count(db, "prod", "web", "nobody");

    seed_launched(db, "t-cancel").await;
    seed_launched(db, "t-abandoned").await;
    seed_launched(db, "t-rerun").await;
    seed(
        db,
        "t-unlaunched",
        RunStatus::Running,
        &[("bump", "prod", "web", StepStatus::Ready, &[])],
        &[],
    )
    .await;
    assert_eq!(web().await, 4, "all four in flight before any teardown");

    for run in ["t-cancel", "t-abandoned", "t-unlaunched"] {
        assert!(cancel_run_request(db, &clock, &RunId(run.into()), None)
            .await
            .unwrap());
    }
    rerun_step(
        db,
        &clock,
        &RunId("t-rerun".into()),
        &StepId("bump".into()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        db.run_status(&RunId("t-cancel".into())).await.unwrap(),
        Some(RunStatus::Cancelled)
    );
    assert_eq!(
        web().await,
        3,
        "the cancelled and the superseded requesters hold until their teardown is retired; \
         the unlaunched one never had a unit"
    );

    let cancel = teardown_of(db, "t-cancel", CANCEL_ATTEMPT).await;
    db.mark_dispatched(cancel).await.unwrap();
    assert_eq!(web().await, 2, "t-cancel's unit is gone");
    let abandoned = teardown_of(db, "t-abandoned", CANCEL_ATTEMPT).await;
    db.dead_letter_outbox(abandoned).await.unwrap();
    assert_eq!(web().await, 1, "an abandoned teardown holds no longer");
    let rerun = teardown_of(db, "t-rerun", SUPERSEDE_TEARDOWN).await;
    db.mark_dispatched(rerun).await.unwrap();
    assert_eq!(web().await, 0, "t-rerun's superseded unit is gone");
}

#[tokio::test]
async fn pg_a_requester_counts_until_its_teardown_is_retired() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    a_requester_counts_until_its_teardown_is_retired(&db).await;
    tdb.cleanup().await;
}

#[tokio::test]
async fn inmemory_a_requester_counts_until_its_teardown_is_retired() {
    a_requester_counts_until_its_teardown_is_retired(&InMemoryDb::new()).await;
}

/// Run `run` (created at `created_at`, Running): `bump` Succeeded with
/// Attempts started at `attempts`, requesting `prod`/`web` with DeploymentGate
/// `live` (Pending) on it, which needs `live_needs`; `verify` stays Pending.
async fn seed_gated(
    db: &dyn Db,
    run: &str,
    created_at: i64,
    attempts: &[i64],
    live_needs: &[&str],
) {
    let run = RunId(run.into());
    let (bump, verify, live) = (
        StepId("bump".into()),
        StepId("verify".into()),
        StepId("live".into()),
    );
    db.create_run(&run, 2, 1, Timestamp(created_at))
        .await
        .unwrap();
    db.record_transition(&run, RunStatus::Pending, RunStatus::Running)
        .await
        .unwrap();
    for step in [&bump, &verify] {
        db.create_step_run(&run, step, Some(&spec()), &[], Timestamp(created_at))
            .await
            .unwrap();
    }
    for (from, to) in path(StepStatus::Succeeded) {
        db.record_step_transition(&run, &bump, *from, *to)
            .await
            .unwrap();
    }
    for (n, at) in attempts.iter().enumerate() {
        db.record_attempt(
            &run,
            &bump,
            &Attempt {
                id: AttemptId(format!("a{}", n + 1)),
                started_at: Timestamp(*at),
                failure: None,
                failure_detail: None,
                outcome: AttemptOutcome::Succeeded,
            },
        )
        .await
        .unwrap();
    }
    let needs: Vec<StepId> = live_needs.iter().map(|n| StepId((*n).into())).collect();
    db.create_step_run(&run, &live, None, &needs, Timestamp(created_at))
        .await
        .unwrap();
    db.set_step_gate(&run, &live, "deployment", None)
        .await
        .unwrap();
    db.set_run_deployment_requests(
        &run,
        &[RunDeploymentRequest {
            step: bump,
            ordinal: 0,
            org: ORG.into(),
            environment: "prod".into(),
            target_source: "argocd".into(),
            target_id: "web".into(),
            version_result: "version".into(),
            evidence_results: Vec::new(),
            gate_steps: vec![live],
        }],
    )
    .await
    .unwrap();
}

/// A Run counted only through a DeploymentGate that is **not reached** (it
/// needs `verify`, still Pending) stops counting once its request's
/// effective time — the requesting Step's LATEST Attempt start, or the Run's
/// creation when it has none — is no longer after the cutoff (R3: two Runs
/// holding targets in opposite orders would otherwise wait forever). A
/// REACHED gate counts whatever the cutoff: its verdict, bounded by its own
/// deadline, ends it.
async fn an_unreached_gate_stops_counting_at_the_cutoff(db: &dyn Db) {
    // Unreached; Attempts at 200 and 500 → effective 500.
    seed_gated(db, "u-attempt", 100, &[200, 500], &["verify"]).await;
    // Unreached; no Attempt row → effective = created 300.
    seed_gated(db, "u-created", 300, &[], &["bump", "verify"]).await;
    // Reached (its one need, `bump`, Succeeded); effective 50.
    seed_gated(db, "reached", 10, &[50], &["bump"]).await;

    let at = |cutoff: i64| count_at(db, "prod", "web", "nobody", Timestamp(cutoff));
    assert_eq!(at(0).await, 3, "every Run inside the deadline counts");
    assert_eq!(at(299).await, 3);
    assert_eq!(
        at(300).await,
        2,
        "no Attempt: the Run's creation is the effective time"
    );
    assert_eq!(
        at(499).await,
        2,
        "the latest Attempt, not the first, is the effective time"
    );
    assert_eq!(
        at(500).await,
        1,
        "past the cutoff an unreached gate stops counting"
    );
    assert_eq!(
        at(i64::MAX).await,
        1,
        "a reached gate counts until its verdict, whatever the cutoff"
    );
}

#[tokio::test]
async fn pg_an_unreached_gate_stops_counting_at_the_cutoff() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    an_unreached_gate_stops_counting_at_the_cutoff(&db).await;
    tdb.cleanup().await;
}

#[tokio::test]
async fn inmemory_an_unreached_gate_stops_counting_at_the_cutoff() {
    an_unreached_gate_stops_counting_at_the_cutoff(&InMemoryDb::new()).await;
}

/// A cancel that dies part-way still holds the target, and still tears the
/// unit down (review correctness MED1). The cancel used to enqueue its
/// teardown intent LAST — after the deploy Step went
/// `Cancelled` and the Run transitioned. In between, the Step no longer
/// counted as in flight and no intent counted for it either, so a held Run
/// was promoted while the holder's Pod still ran; and a crash there lost the
/// intent for good (a re-sent cancel finds the Run terminal and does
/// nothing): no teardown, and no hold. The crash is constructed by refusing
/// the Run's `RunTransitioned` journal write — the last write of the cancel.
async fn a_cancel_that_dies_part_way_still_holds(db: &dyn Db, arm: impl FnOnce()) {
    use scarab_engine::{cancel_run_request, CANCEL_ATTEMPT};
    let clock = scarab_testkit::FakeClock::new(10);
    let web = || count(db, "prod", "web", "nobody");
    let run = RunId("t-crash".into());
    seed_launched(db, "t-crash").await;
    assert_eq!(web().await, 1);

    arm();
    assert!(
        cancel_run_request(db, &clock, &run, None).await.is_err(),
        "the crash was injected"
    );
    assert_eq!(
        web().await,
        1,
        "the holder's unit may still be running: it holds until its teardown is retired"
    );
    let cancel = teardown_of(db, "t-crash", CANCEL_ATTEMPT).await;
    db.mark_dispatched(cancel).await.unwrap();
    assert_eq!(web().await, 0, "its unit is gone");
}

#[tokio::test]
async fn pg_a_cancel_that_dies_part_way_still_holds() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    sqlx::query(
        "CREATE FUNCTION test_refuse_run_transitioned() RETURNS trigger AS $$
         BEGIN
           IF NEW.payload ? 'RunTransitioned' THEN
             RAISE EXCEPTION 'injected: crash before the run journal';
           END IF;
           RETURN NEW;
         END $$ LANGUAGE plpgsql",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER test_refuse_run_transitioned BEFORE INSERT ON events
         FOR EACH ROW EXECUTE FUNCTION test_refuse_run_transitioned()",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();
    a_cancel_that_dies_part_way_still_holds(&db, || {}).await;
    tdb.cleanup().await;
}

#[tokio::test]
async fn inmemory_a_cancel_that_dies_part_way_still_holds() {
    let db = InMemoryDb::new();
    a_cancel_that_dies_part_way_still_holds(&db, || {
        db.fail_next_event_append(|p| {
            matches!(p, scarab_engine::EventPayload::RunTransitioned { .. })
        })
    })
    .await;
}

/// A rerun that dies part-way still holds the target (review correctness
/// MED2). The rerun moved the in-flight deploy Step `Running → Pending` (no
/// longer counted as in flight) and only after its whole re-arm loop wrote
/// the `supersede_teardown` intent that keeps it holding until its Pod is
/// gone: a held Run could be promoted in between, and a crash in between left
/// no teardown and no hold — the superseded Pod ran on beside the next one.
/// The crash is constructed by refusing the `AttemptSuperseded` journal
/// write: [`rerun_crash_seed`], arm, [`rerun_crash_dies`], disarm,
/// [`rerun_crash_converges`].
async fn rerun_crash_seed(db: &dyn Db) {
    seed_launched(db, "t-rerun-crash").await;
    assert_eq!(count(db, "prod", "web", "nobody").await, 1);
}

async fn rerun_crash_dies(db: &dyn Db) {
    let clock = scarab_testkit::FakeClock::new(10);
    let (run, bump) = (RunId("t-rerun-crash".into()), StepId("bump".into()));
    assert!(
        scarab_engine::rerun_step(db, &clock, &run, &bump, None)
            .await
            .is_err(),
        "the crash was injected"
    );
    let bump_now = &db.steps_of_run(&run).await.unwrap()[0];
    assert_eq!(
        (bump_now.status, bump_now.attempts[0].outcome),
        (StepStatus::Running, AttemptOutcome::Running),
        "the re-arm, its stamp and its journal are one commit (git-bug 586187a): all rolled back"
    );
    assert_eq!(
        count(db, "prod", "web", "nobody").await,
        1,
        "the superseded unit may still be running: it holds until its teardown is retired"
    );
}

async fn rerun_crash_converges(db: &dyn Db) {
    let clock = scarab_testkit::FakeClock::new(20);
    let (run, bump) = (RunId("t-rerun-crash".into()), StepId("bump".into()));
    let web = || count(db, "prod", "web", "nobody");
    // The re-sent rerun converges onto the same teardown intent.
    scarab_engine::rerun_step(db, &clock, &run, &bump, None)
        .await
        .unwrap();
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Pending, "re-armed");
    assert_eq!(web().await, 1, "still holding until the unit is gone");
    let teardown = teardown_of(db, "t-rerun-crash", scarab_engine::SUPERSEDE_TEARDOWN).await;
    db.mark_dispatched(teardown).await.unwrap();
    assert_eq!(web().await, 0, "its unit is gone");
}

#[tokio::test]
async fn pg_a_rerun_that_dies_part_way_still_holds() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    rerun_crash_seed(&db).await;
    sqlx::query(
        "CREATE FUNCTION test_refuse_attempt_superseded() RETURNS trigger AS $$
         BEGIN
           IF NEW.payload ? 'AttemptSuperseded' THEN
             RAISE EXCEPTION 'injected: crash before the supersession journal';
           END IF;
           RETURN NEW;
         END $$ LANGUAGE plpgsql",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER test_refuse_attempt_superseded BEFORE INSERT ON events
         FOR EACH ROW EXECUTE FUNCTION test_refuse_attempt_superseded()",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();
    rerun_crash_dies(&db).await;
    sqlx::query("DROP TRIGGER test_refuse_attempt_superseded ON events")
        .execute(&tdb.pool)
        .await
        .unwrap();
    rerun_crash_converges(&db).await;
    tdb.cleanup().await;
}

#[tokio::test]
async fn inmemory_a_rerun_that_dies_part_way_still_holds() {
    let db = InMemoryDb::new();
    rerun_crash_seed(&db).await;
    db.fail_next_event_append(|p| {
        matches!(p, scarab_engine::EventPayload::AttemptSuperseded { .. })
    });
    rerun_crash_dies(&db).await;
    rerun_crash_converges(&db).await;
}
