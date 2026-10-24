//! `Db::deployment_requesting_runs` (slice b, criterion 4): how many Runs —
//! other than the asking one — are **in flight** on a DeploymentTarget, which
//! is what the per-target concurrency hold counts.
//!
//! In flight = the Run is not terminal and a Step of it requesting the target
//! is `Ready`/`Running`, or `Succeeded` with a DeploymentGate on that request
//! not yet terminal — reached (every need of it Succeeded), or not reached
//! and its requesting Step's latest Attempt start after the caller's cutoff. Distinct Runs,
//! scoped to (org, environment, source, id).
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
