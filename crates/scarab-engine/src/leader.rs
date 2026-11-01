//! The leader's view of the store (git-bug aaf4e3e).
//!
//! The scheduler's leader-only legs — admission, the unfinished-creation
//! sweep, shared-service reconciliation — run against a [`LeaderDb`] instead
//! of the plain [`Db`]. Every write they make through it runs under
//! [`Db::fenced_write`] on the [`LeaderTerm`] the leg started with, so a
//! leader paused past its lease TTL that resumes mid-leg has each later write
//! refused with [`DbError::NotLeader`] instead of acting next to the peer that
//! took over. Reads pass through unfenced. The four Db methods only a leader
//! calls take the term themselves and fence in their own transaction.
//!
//! Writing the view as a full [`Db`] implementation, not as a list of call
//! sites, is the point: the legs share helpers (run and step transitions,
//! cancels, gate resolution) with non-leader paths, and every write those
//! helpers make on the leader's behalf is fenced without threading a term
//! through them. A new [`Db`] method does not compile here until it is
//! classified as a read or a fenced write.
//!
//! What it cannot fence: executor side effects a leg makes (launching or
//! tearing down a shared service, cancelling a Pod). Those are idempotent on
//! their fence and do not go through the store.

use async_trait::async_trait;

use crate::ports::*;
use crate::{
    Attempt, AttemptId, ConcurrencyPolicy, DbError, EventKind, FailureKind, LogChunkMeta, OutboxId,
    OutboxMessage, RunId, RunListFilter, RunService, RunStatus, RunSummary, ServiceStatus, StepId,
    StepRun, StepSpec, StepStatus, Timestamp,
};

/// The store as the leader of one lease term sees it: reads pass through,
/// writes are fenced on [`term`](LeaderDb::term).
pub struct LeaderDb<'a> {
    inner: &'a dyn Db,
    term: LeaderTerm,
}

impl<'a> LeaderDb<'a> {
    pub fn new(inner: &'a dyn Db, term: LeaderTerm) -> Self {
        Self { inner, term }
    }

    /// The lease term every write through this view is fenced on.
    pub fn term(&self) -> &LeaderTerm {
        &self.term
    }

    /// Run one write under the fence and hand back its result.
    async fn fence<T: Send>(
        &self,
        write: impl std::future::Future<Output = Result<T, DbError>> + Send,
    ) -> Result<T, DbError> {
        let mut out = None;
        self.inner
            .fenced_write(
                &self.term,
                Box::pin(async {
                    out = Some(write.await?);
                    Ok(())
                }),
            )
            .await?;
        out.ok_or_else(|| DbError::Other("fenced write finished without a result".into()))
    }
}

#[async_trait]
impl Db for LeaderDb<'_> {
    async fn claim_ready_steps(&self, leader: &LeaderTerm, limit: u32) -> Result<Vec<StepRun>, DbError> {
        self.inner.claim_ready_steps(leader, limit).await
    }

    async fn create_run(&self, run: &RunId, ir_version: u32, event_schema_version: u32, at: Timestamp) -> Result<(), DbError> {
        self.fence(self.inner.create_run(run, ir_version, event_schema_version, at)).await
    }

    async fn begin_run_creation(&self, run: &RunId, ir_version: u32, event_schema_version: u32, at: Timestamp) -> Result<(), DbError> {
        self.fence(self.inner.begin_run_creation(run, ir_version, event_schema_version, at)).await
    }

    async fn finish_run_creation(&self, run: &RunId) -> Result<(), DbError> {
        self.fence(self.inner.finish_run_creation(run)).await
    }

    async fn run_creation_unfinished(&self, run: &RunId) -> Result<bool, DbError> {
        self.inner.run_creation_unfinished(run).await
    }

    async fn unfinished_creations_before(&self, created_before: Timestamp) -> Result<Vec<RunId>, DbError> {
        self.inner.unfinished_creations_before(created_before).await
    }

    async fn create_step_run(&self, run: &RunId, step: &StepId, spec: Option<&StepSpec>, needs: &[StepId], at: Timestamp) -> Result<(), DbError> {
        self.fence(self.inner.create_step_run(run, step, spec, needs, at)).await
    }

    async fn store_run_ir(&self, run: &RunId, ir: &serde_json::Value) -> Result<(), DbError> {
        self.fence(self.inner.store_run_ir(run, ir)).await
    }

    async fn run_ir(&self, run: &RunId) -> Result<Option<serde_json::Value>, DbError> {
        self.inner.run_ir(run).await
    }

    async fn set_run_params(&self, run: &RunId, params: &std::collections::BTreeMap<String, serde_json::Value>) -> Result<(), DbError> {
        self.fence(self.inner.set_run_params(run, params)).await
    }

    async fn run_params(&self, run: &RunId) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError> {
        self.inner.run_params(run).await
    }

    async fn set_run_event_facts(&self, run: &RunId, facts: &serde_json::Map<String, serde_json::Value>) -> Result<(), DbError> {
        self.fence(self.inner.set_run_event_facts(run, facts)).await
    }

    async fn run_event_facts(&self, run: &RunId) -> Result<serde_json::Map<String, serde_json::Value>, DbError> {
        self.inner.run_event_facts(run).await
    }

    async fn set_run_deploy_context(&self, run: &RunId, ctx: &crate::DeployContext) -> Result<(), DbError> {
        self.fence(self.inner.set_run_deploy_context(run, ctx)).await
    }

    async fn run_deploy_context(&self, run: &RunId) -> Result<Option<crate::DeployContext>, DbError> {
        self.inner.run_deploy_context(run).await
    }

    async fn mark_deploy_run(&self, run: &RunId, steps: &[StepId]) -> Result<(), DbError> {
        self.fence(self.inner.mark_deploy_run(run, steps)).await
    }

    async fn deploy_steps_of_run(&self, run: &RunId) -> Result<Vec<StepId>, DbError> {
        self.inner.deploy_steps_of_run(run).await
    }

    async fn set_step_output(&self, run: &RunId, step: &StepId, attempt: &AttemptId, snapshot: &str, identity: Option<&str>) -> Result<(), DbError> {
        self.fence(self.inner.set_step_output(run, step, attempt, snapshot, identity)).await
    }

    async fn step_output(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        self.inner.step_output(run, step).await
    }

    async fn step_output_identity(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        self.inner.step_output_identity(run, step).await
    }

    async fn attempt_output(&self, run: &RunId, step: &StepId, attempt: &AttemptId) -> Result<Option<String>, DbError> {
        self.inner.attempt_output(run, step, attempt).await
    }

    async fn set_step_results(&self, run: &RunId, step: &StepId, attempt: &AttemptId, results: &std::collections::BTreeMap<String, serde_json::Value>) -> Result<(), DbError> {
        self.fence(self.inner.set_step_results(run, step, attempt, results)).await
    }

    async fn ingest_attempt_results(&self, run: &RunId, step: &StepId, attempt: &AttemptId, results: &std::collections::BTreeMap<String, serde_json::Value>) -> Result<(), DbError> {
        self.fence(self.inner.ingest_attempt_results(run, step, attempt, results)).await
    }

    async fn step_results(&self, run: &RunId, step: &StepId) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError> {
        self.inner.step_results(run, step).await
    }

    async fn attempt_results(&self, run: &RunId, step: &StepId, attempt: &AttemptId) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError> {
        self.inner.attempt_results(run, step, attempt).await
    }

    async fn step_evidence_attempt(&self, run: &RunId, step: &StepId) -> Result<Option<AttemptId>, DbError> {
        self.inner.step_evidence_attempt(run, step).await
    }

    async fn set_attempt_consumed(&self, run: &RunId, step: &StepId, attempt: &AttemptId, consumed: &std::collections::BTreeMap<String, String>) -> Result<(), DbError> {
        self.fence(self.inner.set_attempt_consumed(run, step, attempt, consumed)).await
    }

    async fn attempt_consumed(&self, run: &RunId, step: &StepId, attempt: &AttemptId) -> Result<std::collections::BTreeMap<String, String>, DbError> {
        self.inner.attempt_consumed(run, step, attempt).await
    }

    async fn set_attempt_workspace_input_steps(&self, run: &RunId, step: &StepId, attempt: &AttemptId, steps: &[StepId]) -> Result<(), DbError> {
        self.fence(self.inner.set_attempt_workspace_input_steps(run, step, attempt, steps)).await
    }

    async fn attempt_workspace_input_steps(&self, run: &RunId, step: &StepId, attempt: &AttemptId) -> Result<Option<Vec<StepId>>, DbError> {
        self.inner.attempt_workspace_input_steps(run, step, attempt).await
    }

    async fn set_step_input(&self, run: &RunId, step: &StepId, signature: Option<&str>) -> Result<(), DbError> {
        self.fence(self.inner.set_step_input(run, step, signature)).await
    }

    async fn step_input(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        self.inner.step_input(run, step).await
    }

    async fn set_step_inputs(&self, run: &RunId, step: &StepId, inputs: &[StepId]) -> Result<(), DbError> {
        self.fence(self.inner.set_step_inputs(run, step, inputs)).await
    }

    async fn step_inputs(&self, run: &RunId, step: &StepId) -> Result<Option<Vec<StepId>>, DbError> {
        self.inner.step_inputs(run, step).await
    }

    async fn set_run_concurrency(&self, run: &RunId, group: &str, policy: ConcurrencyPolicy) -> Result<(), DbError> {
        self.fence(self.inner.set_run_concurrency(run, group, policy)).await
    }

    async fn run_concurrency(&self, run: &RunId) -> Result<Option<(String, ConcurrencyPolicy)>, DbError> {
        self.inner.run_concurrency(run).await
    }

    async fn acquire_slot(&self, leader: &LeaderTerm, group: &str, run: &RunId) -> Result<Option<RunId>, DbError> {
        self.inner.acquire_slot(leader, group, run).await
    }

    async fn release_slot(&self, group: &str, run: &RunId) -> Result<(), DbError> {
        self.fence(self.inner.release_slot(group, run)).await
    }

    async fn set_supersede_key(&self, run: &RunId, key: &str) -> Result<(), DbError> {
        self.fence(self.inner.set_supersede_key(run, key)).await
    }

    async fn superseded_by(&self, run: &RunId) -> Result<Vec<RunId>, DbError> {
        self.inner.superseded_by(run).await
    }

    async fn set_run_scheduling(&self, run: &RunId, project: &str, priority: i32) -> Result<(), DbError> {
        self.fence(self.inner.set_run_scheduling(run, project, priority)).await
    }

    async fn run_project(&self, run: &RunId) -> Result<Option<String>, DbError> {
        self.inner.run_project(run).await
    }

    async fn set_run_tenant(&self, run: &RunId, org: &str, project: &str) -> Result<(), DbError> {
        self.fence(self.inner.set_run_tenant(run, org, project)).await
    }

    async fn run_tenant(&self, run: &RunId) -> Result<Option<(String, String)>, DbError> {
        self.inner.run_tenant(run).await
    }

    async fn run_tenants(&self, runs: &[RunId]) -> Result<std::collections::HashMap<RunId, (String, String)>, DbError> {
        self.inner.run_tenants(runs).await
    }

    async fn allocate_run_number(&self, run: &RunId, org: &str, project: &str) -> Result<i64, DbError> {
        self.fence(self.inner.allocate_run_number(run, org, project)).await
    }

    async fn run_number(&self, run: &RunId) -> Result<Option<i64>, DbError> {
        self.inner.run_number(run).await
    }

    async fn set_run_origin(&self, run: &RunId, trigger_kind: &str, actor: Option<&str>, git_ref: Option<&str>, sha: Option<&str>, pr_number: Option<i64>, pr_base: Option<&str>) -> Result<(), DbError> {
        self.fence(self.inner.set_run_origin(run, trigger_kind, actor, git_ref, sha, pr_number, pr_base)).await
    }

    async fn run_pr_base(&self, run: &RunId) -> Result<Option<String>, DbError> {
        self.inner.run_pr_base(run).await
    }

    async fn set_run_pipeline(&self, run: &RunId, pipeline: &str) -> Result<(), DbError> {
        self.fence(self.inner.set_run_pipeline(run, pipeline)).await
    }

    async fn run_pipeline(&self, run: &RunId) -> Result<Option<String>, DbError> {
        self.inner.run_pipeline(run).await
    }

    async fn set_run_trigger_title(&self, run: &RunId, title: &str) -> Result<(), DbError> {
        self.fence(self.inner.set_run_trigger_title(run, title)).await
    }

    async fn run_trigger_title(&self, run: &RunId) -> Result<Option<String>, DbError> {
        self.inner.run_trigger_title(run).await
    }

    async fn cache_record(&self, project: &str, key: &str, dir: &str, tree_root: &str, run: &RunId, step: &StepId, attempt: &AttemptId, now: Timestamp) -> Result<(), DbError> {
        self.fence(self.inner.cache_record(project, key, dir, tree_root, run, step, attempt, now)).await
    }

    async fn cache_lookup(&self, project: &str, key: &str) -> Result<Vec<(String, String)>, DbError> {
        self.inner.cache_lookup(project, key).await
    }

    async fn run_pr_context(&self, run: &RunId) -> Result<bool, DbError> {
        self.inner.run_pr_context(run).await
    }

    async fn count_in_flight_runs(&self, project: Option<&str>) -> Result<u32, DbError> {
        self.inner.count_in_flight_runs(project).await
    }

    async fn set_step_gate(&self, run: &RunId, step: &StepId, kind: &str, timer_seconds: Option<i64>) -> Result<(), DbError> {
        self.fence(self.inner.set_step_gate(run, step, kind, timer_seconds)).await
    }

    async fn gate_timer_seconds(&self, run: &RunId, step: &StepId) -> Result<Option<i64>, DbError> {
        self.inner.gate_timer_seconds(run, step).await
    }

    async fn set_gate_releaser(&self, run: &RunId, step: &StepId, releaser: &str) -> Result<(), DbError> {
        self.fence(self.inner.set_gate_releaser(run, step, releaser)).await
    }

    async fn gate_releaser(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        self.inner.gate_releaser(run, step).await
    }

    async fn set_gate_wait_inputs(&self, leader: &LeaderTerm, run: &RunId, step: &StepId, attempt: &AttemptId, inputs: &serde_json::Value) -> Result<bool, DbError> {
        self.inner.set_gate_wait_inputs(leader, run, step, attempt, inputs).await
    }

    async fn gate_wait_inputs(&self, run: &RunId, step: &StepId, attempt: &AttemptId) -> Result<Option<serde_json::Value>, DbError> {
        self.inner.gate_wait_inputs(run, step, attempt).await
    }

    async fn pending_plugin_gates(&self, org: &str, releaser: &str, after: Option<&crate::PendingGateKey>, limit: u32) -> Result<Vec<crate::PendingGateWait>, DbError> {
        self.inner.pending_plugin_gates(org, releaser, after, limit).await
    }

    async fn create_run_service(&self, run: &RunId, take: i64, name: &str, at: Timestamp) -> Result<(), DbError> {
        self.fence(self.inner.create_run_service(run, take, name, at)).await
    }

    async fn set_run_service(&self, run: &RunId, take: i64, name: &str, status: ServiceStatus, handle: Option<&str>) -> Result<(), DbError> {
        self.fence(self.inner.set_run_service(run, take, name, status, handle)).await
    }

    async fn run_services(&self, run: &RunId) -> Result<Vec<RunService>, DbError> {
        self.inner.run_services(run).await
    }

    async fn run_status(&self, run: &RunId) -> Result<Option<RunStatus>, DbError> {
        self.inner.run_status(run).await
    }

    async fn run_reopen_generation(&self, run: &RunId) -> Result<u64, DbError> {
        self.inner.run_reopen_generation(run).await
    }

    async fn active_runs(&self) -> Result<Vec<RunId>, DbError> {
        self.inner.active_runs().await
    }

    async fn list_runs(&self, limit: u32) -> Result<Vec<RunSummary>, DbError> {
        self.inner.list_runs(limit).await
    }

    async fn list_runs_for_tenant(&self, org: &str, project: &str, filter: &RunListFilter, limit: u32) -> Result<Vec<RunSummary>, DbError> {
        self.inner.list_runs_for_tenant(org, project, filter, limit).await
    }

    async fn record_refused_run(&self, refused: &crate::RefusedRun) -> Result<(), DbError> {
        self.fence(self.inner.record_refused_run(refused)).await
    }

    async fn refused_runs_for_tenant(&self, org: &str, project: &str, limit: u32) -> Result<Vec<crate::RefusedRun>, DbError> {
        self.inner.refused_runs_for_tenant(org, project, limit).await
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventKind>, DbError> {
        self.inner.events(run).await
    }

    async fn append_log_chunk(&self, run: &RunId, step: &StepId, attempt: &AttemptId, meta: &LogChunkMeta) -> Result<(), DbError> {
        self.fence(self.inner.append_log_chunk(run, step, attempt, meta)).await
    }

    async fn log_chunks(&self, run: &RunId, step: &StepId, attempt: &AttemptId) -> Result<Vec<LogChunkMeta>, DbError> {
        self.inner.log_chunks(run, step, attempt).await
    }

    async fn steps_of_run(&self, run: &RunId) -> Result<Vec<StepRun>, DbError> {
        self.inner.steps_of_run(run).await
    }

    async fn steps_of_runs(&self, runs: &[RunId]) -> Result<std::collections::HashMap<RunId, Vec<StepRun>>, DbError> {
        self.inner.steps_of_runs(runs).await
    }

    async fn step_spec(&self, run: &RunId, step: &StepId) -> Result<Option<StepSpec>, DbError> {
        self.inner.step_spec(run, step).await
    }

    async fn record_transition(&self, run: &RunId, from: RunStatus, to: RunStatus) -> Result<(), DbError> {
        self.fence(self.inner.record_transition(run, from, to)).await
    }

    async fn commit_run_transition(&self, run: &RunId, from: RunStatus, to: RunStatus, generation: u64, events: &[EventKind], outbox: &[OutboxMessage]) -> Result<(), DbError> {
        self.fence(self.inner.commit_run_transition(run, from, to, generation, events, outbox)).await
    }

    async fn record_step_transition(&self, run: &RunId, step: &StepId, from: StepStatus, to: StepStatus) -> Result<(), DbError> {
        self.fence(self.inner.record_step_transition(run, step, from, to)).await
    }

    async fn commit_step_transition(&self, run: &RunId, step: &StepId, from: StepStatus, to: StepStatus, frontier: crate::AttemptFrontier<'_>, stamp: Option<(&AttemptId, &crate::AttemptStamp)>, events: &[EventKind], outbox: &[OutboxMessage]) -> Result<(), DbError> {
        self.fence(self.inner.commit_step_transition(run, step, from, to, frontier, stamp, events, outbox)).await
    }

    async fn record_attempt(&self, run: &RunId, step: &StepId, attempt: &Attempt) -> Result<(), DbError> {
        self.fence(self.inner.record_attempt(run, step, attempt)).await
    }

    async fn attempts_of_step(&self, run: &RunId, step: &StepId) -> Result<Vec<Attempt>, DbError> {
        self.inner.attempts_of_step(run, step).await
    }

    async fn set_attempt_handle(&self, run: &RunId, step: &StepId, attempt: &AttemptId, handle: &str) -> Result<(), DbError> {
        self.fence(self.inner.set_attempt_handle(run, step, attempt, handle)).await
    }

    async fn attempt_handle(&self, run: &RunId, step: &StepId, attempt: &AttemptId) -> Result<Option<String>, DbError> {
        self.inner.attempt_handle(run, step, attempt).await
    }

    async fn set_attempt_failure(&self, run: &RunId, step: &StepId, attempt: &AttemptId, failure: FailureKind, detail: Option<&str>) -> Result<(), DbError> {
        self.fence(self.inner.set_attempt_failure(run, step, attempt, failure, detail)).await
    }

    async fn set_attempt_outcome(&self, run: &RunId, step: &StepId, attempt: &AttemptId, outcome: crate::AttemptOutcome) -> Result<(), DbError> {
        self.fence(self.inner.set_attempt_outcome(run, step, attempt, outcome)).await
    }

    async fn append_event(&self, event: &EventKind) -> Result<(), DbError> {
        self.fence(self.inner.append_event(event)).await
    }

    async fn open_gate_wait(&self, leader: &LeaderTerm, run: &RunId, step: &StepId, started_at: Timestamp, at: Timestamp) -> Result<Option<Attempt>, DbError> {
        self.inner.open_gate_wait(leader, run, step, started_at, at).await
    }

    async fn resolve_gate_wait(&self, run: &RunId, step: &StepId, res: &crate::GateResolution) -> Result<Option<AttemptId>, DbError> {
        self.fence(self.inner.resolve_gate_wait(run, step, res)).await
    }

    async fn close_running_attempt(&self, run: &RunId, step: &StepId, attempt: &AttemptId, outcome: crate::AttemptOutcome, event: Option<&EventKind>) -> Result<bool, DbError> {
        self.fence(self.inner.close_running_attempt(run, step, attempt, outcome, event)).await
    }

    async fn enqueue_outbox(&self, msg: &OutboxMessage) -> Result<(), DbError> {
        self.fence(self.inner.enqueue_outbox(msg)).await
    }

    async fn claim_outbox(&self, owner: &str, kind: Option<&str>, limit: u32, visibility_ms: i64) -> Result<Vec<OutboxMessage>, DbError> {
        self.fence(self.inner.claim_outbox(owner, kind, limit, visibility_ms)).await
    }

    async fn renew_outbox_claim(&self, id: OutboxId, owner: &str, visibility_ms: i64) -> Result<bool, DbError> {
        self.fence(self.inner.renew_outbox_claim(id, owner, visibility_ms)).await
    }

    async fn mark_dispatched(&self, id: OutboxId) -> Result<(), DbError> {
        self.fence(self.inner.mark_dispatched(id)).await
    }

    async fn outbox_key_pending(&self, idempotency_key: &str) -> Result<bool, DbError> {
        self.inner.outbox_key_pending(idempotency_key).await
    }

    async fn record_outbox_failure(&self, id: OutboxId) -> Result<u32, DbError> {
        self.fence(self.inner.record_outbox_failure(id)).await
    }

    async fn dead_letter_outbox(&self, id: OutboxId) -> Result<(), DbError> {
        self.fence(self.inner.dead_letter_outbox(id)).await
    }

    async fn prunable_log_runs(&self, cutoff: Timestamp, limit: u32) -> Result<Vec<RunId>, DbError> {
        self.inner.prunable_log_runs(cutoff, limit).await
    }

    async fn log_object_keys_of_run(&self, run: &RunId) -> Result<Vec<String>, DbError> {
        self.inner.log_object_keys_of_run(run).await
    }

    async fn delete_log_index_of_run(&self, run: &RunId) -> Result<(), DbError> {
        self.fence(self.inner.delete_log_index_of_run(run)).await
    }

    async fn run_status_counts(&self) -> Result<Vec<(String, u64)>, DbError> {
        self.inner.run_status_counts().await
    }

    async fn outbox_depth(&self) -> Result<u64, DbError> {
        self.inner.outbox_depth().await
    }

    async fn put_artifacts(&self, run: &RunId, step: &StepId, attempt: &AttemptId, succeeded: bool, artifacts: &[crate::ArtifactMeta], at: Timestamp) -> Result<(), DbError> {
        self.fence(self.inner.put_artifacts(run, step, attempt, succeeded, artifacts, at)).await
    }

    async fn artifacts_of_run(&self, run: &RunId) -> Result<Vec<crate::ArtifactRecord>, DbError> {
        self.inner.artifacts_of_run(run).await
    }

    async fn prunable_artifact_runs(&self, cutoff: Timestamp, limit: u32) -> Result<Vec<RunId>, DbError> {
        self.inner.prunable_artifact_runs(cutoff, limit).await
    }

    async fn delete_artifacts_of_run(&self, run: &RunId) -> Result<(), DbError> {
        self.fence(self.inner.delete_artifacts_of_run(run)).await
    }

    async fn gc_workspace_roots(&self, terminal_cutoff: Timestamp) -> Result<Vec<(String, Timestamp)>, DbError> {
        self.fence(self.inner.gc_workspace_roots(terminal_cutoff)).await
    }

    async fn pin_run_snapshots(&self, run: &RunId, by: Option<&str>, at: Timestamp) -> Result<bool, DbError> {
        self.fence(self.inner.pin_run_snapshots(run, by, at)).await
    }

    async fn unpin_run_snapshots(&self, run: &RunId) -> Result<bool, DbError> {
        self.fence(self.inner.unpin_run_snapshots(run)).await
    }

    async fn run_snapshot_retention(&self, run: &RunId) -> Result<Option<SnapshotRetention>, DbError> {
        self.inner.run_snapshot_retention(run).await
    }

    async fn forget_workspace_root(&self, root: &str) -> Result<u32, DbError> {
        self.fence(self.inner.forget_workspace_root(root)).await
    }

    async fn lease(&self, resource: &str, owner: &str, ttl_ms: i64) -> Result<Lease, DbError> {
        self.inner.lease(resource, owner, ttl_ms).await
    }

    async fn fenced_write<'a>(&'a self, leader: &'a LeaderTerm, write: FencedWrite<'a>) -> Result<(), DbError> {
        self.inner.fenced_write(leader, write).await
    }

}
