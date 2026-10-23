//! Ticket 4cf03d7 (ADR-0064 control-plane half): an executor-reported failure
//! CAUSE is threaded, not dropped — `ExecState::Failed { cause }` must land as
//! `failure_detail` on the attempt row AND as `cause` on the persisted
//! `AttemptFinished` event. Driven over the in-memory store so it runs without
//! Postgres (the PG column round-trip is `db_contract.rs`; the two stores'
//! `set_attempt_failure` are contract twins).
//!
//! The mutation this file kills: any link of the chain
//! `poll → settle_failed_attempt(cause) → set_attempt_failure(detail)` /
//! `→ finalize_step(cause) → AttemptFinished { cause }` quietly passing `None`
//! instead of the reported cause. Every link compiles either way — only the
//! end-to-end assertion notices.

use scarab_engine::ports::{ExecState, FailureClass};
use scarab_engine::{
    Db, EventPayload, FailureKind, RunId, Scheduler, StepId, StepSpec, Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

const CAUSE: &str = "cold tier refused: connection refused (flush: depot 503)";

fn spec() -> StepSpec {
    serde_json::from_value(serde_json::json!({
        "image": "img",
        "command": ["x"],
        "env": [],
    }))
    .unwrap()
}

/// A post-start infra failure with a cause (the EvidenceLost shape the k8s
/// executor reports on a dead Depot) settles with the cause on BOTH durable
/// grains: the attempt row's `failure_detail` and the `AttemptFinished`
/// event's `cause`. No `retry:` is configured, so the attempt finalizes on
/// the first failure — the `finalize_step` leg of the settle path.
#[tokio::test]
async fn a_reported_cause_lands_on_the_attempt_row_and_the_event() {
    let db = InMemoryDb::new();
    let run = RunId("r1".into());
    let step = StepId("s".into());
    db.create_run(&run, 1, 1, Timestamp(1_000)).await.unwrap();
    db.create_step_run(&run, &step, Some(&spec()), &[], Timestamp(1_000))
        .await
        .unwrap();
    db.store_run_ir(
        &run,
        &serde_json::json!({ "ir_version": 1, "steps": [{ "id": "s", "image": "img" }] }),
    )
    .await
    .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Failed {
        exit_code: None,
        class: FailureClass::Infra {
            never_started: false,
        },
        cause: Some(CAUSE.into()),
    });

    // Launch, observe the failure, settle, and let the run finalize.
    for _ in 0..3 {
        Scheduler::new(&db, &clock, &exec, "sched")
            .with_outbox_visibility_ms(0)
            .tick(&run)
            .await
            .unwrap();
    }

    // The attempt row carries class AND cause (InMemoryDb twin of the PG
    // `failure_detail` column — kills the twin ignoring its `detail` param).
    let attempts = db.attempts_of_step(&run, &step).await.unwrap();
    assert_eq!(attempts.len(), 1, "no retry: was configured");
    assert_eq!(
        attempts[0].failure,
        Some(FailureKind::Infra {
            never_started: false
        })
    );
    assert_eq!(
        attempts[0].failure_detail.as_deref(),
        Some(CAUSE),
        "the executor's cause must land on the attempt row, not die in a log"
    );

    // The persisted event log carries it too (kills the scheduler emitting
    // `cause: None` on the AttemptFinished it appends in finalize_step).
    let finished: Vec<_> = db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::AttemptFinished {
                step: s,
                failure,
                cause,
                ..
            } if s == step => Some((failure, cause)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![(
            Some(FailureKind::Infra {
                never_started: false
            }),
            Some(CAUSE.to_string())
        )],
        "exactly one AttemptFinished, carrying the cause"
    );
}

/// Refuses every launch by policy — the shape `SecretInjectingExecutor` takes
/// when a Deploy Run's Environment no longer admits its Project (ADR-0071 §2).
/// Everything else is unreachable: a refused step never gets a handle.
struct RefusingExec;

const REFUSAL: &str = "project `web` is no longer eligible to deploy to environment `prod`";

#[async_trait::async_trait]
impl scarab_engine::Executor for RefusingExec {
    async fn infra_condition(
        &self,
        _handle: &scarab_engine::ports::ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, scarab_engine::ExecError> {
        Ok(None)
    }
    async fn workspace_provisioning(
        &self,
        _handle: &scarab_engine::ports::ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, scarab_engine::ExecError> {
        Ok(None)
    }
    async fn launch(
        &self,
        _step: &scarab_engine::StepRun,
        _spec: &StepSpec,
    ) -> Result<scarab_engine::ports::ExecHandle, scarab_engine::ExecError> {
        Err(scarab_engine::ExecError::Refused(REFUSAL.into()))
    }
    async fn poll(
        &self,
        _handle: &scarab_engine::ports::ExecHandle,
    ) -> Result<ExecState, scarab_engine::ExecError> {
        unreachable!("a refused launch has no handle to poll")
    }
    async fn cancel(
        &self,
        _handle: &scarab_engine::ports::ExecHandle,
    ) -> Result<(), scarab_engine::ExecError> {
        Ok(())
    }
}

/// A launch REFUSED by policy is a step verdict, not a scheduler error: the
/// tick succeeds, the step fails `Config` with the refusal as its cause on
/// both durable grains, and the Run settles Failed. Before `ExecError::Refused`
/// the decorator returned `Launch`, the scheduler's `?` turned it into a tick
/// failure, and the Run spun for `tick_failure_deadline_ms` before being
/// dead-lettered with the reason buried (c-full review M1).
#[tokio::test]
async fn a_refused_launch_fails_the_step_config_with_the_reason() {
    let db = InMemoryDb::new();
    let run = RunId("r1".into());
    let step = StepId("s".into());
    db.create_run(&run, 1, 1, Timestamp(1_000)).await.unwrap();
    db.create_step_run(&run, &step, Some(&spec()), &[], Timestamp(1_000))
        .await
        .unwrap();
    db.store_run_ir(
        &run,
        &serde_json::json!({ "ir_version": 1, "steps": [{ "id": "s", "image": "img" }] }),
    )
    .await
    .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = RefusingExec;
    for _ in 0..3 {
        Scheduler::new(&db, &clock, &exec, "sched")
            .with_outbox_visibility_ms(0)
            .tick(&run)
            .await
            .expect("a refusal is a step verdict, never a tick error");
    }

    let attempts = db.attempts_of_step(&run, &step).await.unwrap();
    assert_eq!(attempts.len(), 1, "Config is never auto-retried");
    assert_eq!(attempts[0].failure, Some(FailureKind::Config));
    assert_eq!(attempts[0].failure_detail.as_deref(), Some(REFUSAL));
    let finished: Vec<_> = db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::AttemptFinished {
                step: s,
                failure,
                cause,
                ..
            } if s == step => Some((failure, cause)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![(Some(FailureKind::Config), Some(REFUSAL.to_string()))]
    );
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(scarab_engine::RunStatus::Failed)
    );
}
