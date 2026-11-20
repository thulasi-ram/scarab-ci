//! The value-ledger decorator (git-bug 2d78464): the innermost layer of the
//! executor stack, `Forge(SecretInjecting(Ledger(backend)))`, so it sees the
//! final launch spec, every credential the outer layers resolved included.
//!
//! [`RedactionLedgerExecutor::launch`] records the values the spec mounts
//! ([`StepSpec::mounted_secret_values`]) as a sealed ledger generation in
//! Postgres, then calls the backend, which is the only call that writes
//! them where the unit can read them (I7). A ledger write that fails fails
//! the launch. Every replica redacts the unit's log with the ledger, so the
//! process that launched the unit is not special.
//!
//! [`build_executor_stack`] is the one place the order is set; `main.rs`
//! and the tests call it.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use scarab_engine::ports::{ExecHandle, ExecState, LogChunks, LogFrom, LogSourceRef};
use scarab_engine::{
    Clock, ExecError, Executor, LogSourceState, LogStreamId, RunId, StepRun, StepSpec,
};

use crate::forge_token::ForgeCredentialExecutor;
use crate::logs::{LedgerError, LogService};
use crate::secret_executor::SecretInjectingExecutor;

/// Build the executor stack: `Forge(SecretInjecting(Ledger(backend)))`.
/// `secrets` and `forge` configure the two outer layers over the layer
/// they are given. The ledger layer is innermost, so every value an outer
/// layer adds to the spec is recorded before the backend writes it.
pub fn build_executor_stack(
    backend: Arc<dyn Executor>,
    logs: Arc<LogService>,
    clock: Arc<dyn Clock>,
    secrets: impl FnOnce(Arc<dyn Executor>) -> SecretInjectingExecutor,
    forge: impl FnOnce(Arc<dyn Executor>) -> ForgeCredentialExecutor,
) -> Arc<dyn Executor> {
    let ledger: Arc<dyn Executor> =
        Arc::new(RedactionLedgerExecutor::new(backend, logs.clone(), clock));
    let with_secrets: Arc<dyn Executor> = Arc::new(secrets(ledger));
    Arc::new(forge(with_secrets).with_logs(logs))
}

/// The unit a launch of `step` creates: its current attempt.
pub fn step_unit(step: &StepRun) -> Option<LogStreamId> {
    step.current_attempt()
        .map(|a| LogStreamId::new(&step.run, &step.step, &a.id))
}

/// Records each launch's mounted values in the unit's value ledger before
/// the backend launches it. See the module docs.
pub struct RedactionLedgerExecutor {
    inner: Arc<dyn Executor>,
    logs: Arc<LogService>,
    clock: Arc<dyn Clock>,
    /// Handles whose unit's start is recorded already, so a poll does not
    /// write it again.
    started: Mutex<HashSet<String>>,
}

impl RedactionLedgerExecutor {
    pub fn new(inner: Arc<dyn Executor>, logs: Arc<LogService>, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner,
            logs,
            clock,
            started: Mutex::new(HashSet::new()),
        }
    }

    /// Record that the unit behind `handle` started (first write wins).
    async fn mark_started(&self, handle: &ExecHandle) {
        if self.started.lock().unwrap().contains(&handle.0) {
            return;
        }
        let db = self.logs.db();
        let Ok(Some(unit)) = db.log_unit_by_handle(&handle.0).await else {
            return;
        };
        let now = self.clock.now().await;
        if db.mark_log_unit_started(&unit, now).await.is_ok() {
            self.started.lock().unwrap().insert(handle.0.clone());
        }
    }

    /// Whether `state` means the unit's process ran.
    fn ran(state: &ExecState) -> bool {
        matches!(
            state,
            ExecState::Running
                | ExecState::Succeeded
                | ExecState::Failed {
                    exit_code: Some(_),
                    ..
                }
        )
    }
}

fn ledger_launch_error(e: LedgerError) -> ExecError {
    ExecError::Launch(format!(
        "recording the unit's secret values before launch failed: {e}"
    ))
}

#[async_trait]
impl Executor for RedactionLedgerExecutor {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        let Some(unit) = step_unit(step) else {
            return Err(ExecError::Launch(
                "the step has no attempt to launch".to_string(),
            ));
        };
        let handle = self.inner.unit_handle(step);
        let now = self.clock.now().await;
        self.logs
            .record_launch_values(
                &unit,
                handle.as_ref().map(|h| h.0.as_str()),
                spec.mounted_secret_values(),
                now,
            )
            .await
            .map_err(ledger_launch_error)?;
        // A launch error after the write keeps the generation: its values
        // may be mounted already.
        let launched = self.inner.launch(step, spec).await?;
        if handle.as_ref() != Some(&launched) {
            self.logs
                .db()
                .write_log_unit(&unit, Some(&launched.0), 0, None, now)
                .await
                .map_err(|e| ExecError::Other(format!("recording the unit's handle: {e}")))?;
        }
        Ok(launched)
    }

    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
        let state = self.inner.poll(handle).await?;
        if Self::ran(&state) {
            self.mark_started(handle).await;
        }
        Ok(state)
    }

    async fn cancel(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        // A unit cancelled before any poll saw it run still records its
        // start: poll once first.
        if let Ok(state) = self.inner.poll(handle).await {
            if Self::ran(&state) {
                self.mark_started(handle).await;
            }
        }
        self.inner.cancel(handle).await
    }

    fn unit_handle(&self, step: &StepRun) -> Option<ExecHandle> {
        self.inner.unit_handle(step)
    }

    async fn torn_down(&self, handle: &ExecHandle) -> Result<bool, ExecError> {
        self.inner.torn_down(handle).await
    }

    async fn output(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        self.inner.output(handle).await
    }

    async fn output_identity(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        self.inner.output_identity(handle).await
    }

    async fn results(
        &self,
        handle: &ExecHandle,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ExecError> {
        self.inner.results(handle).await
    }

    async fn results_delivery_failure(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
        self.inner.results_delivery_failure(handle).await
    }

    async fn artifacts(
        &self,
        handle: &ExecHandle,
    ) -> Result<Vec<scarab_engine::ArtifactMeta>, ExecError> {
        self.inner.artifacts(handle).await
    }

    async fn cache_saves(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::CacheSaves>, ExecError> {
        self.inner.cache_saves(handle).await
    }

    async fn workspace_provisioning(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        self.inner.workspace_provisioning(handle).await
    }

    async fn gate_refusal(&self, gate: &StepRun) -> Result<Option<String>, ExecError> {
        self.inner.gate_refusal(gate).await
    }

    async fn infra_condition(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        self.inner.infra_condition(handle).await
    }

    async fn delete_unit_secrets(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        self.inner.delete_unit_secrets(handle).await
    }

    async fn log_source_state(&self, src: &LogSourceRef) -> Result<LogSourceState, ExecError> {
        self.inner.log_source_state(src).await
    }

    async fn log_stream(
        &self,
        step: &StepRun,
        from: LogFrom,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.inner.log_stream(step, from).await
    }

    async fn sidecar_log_stream(
        &self,
        step: &StepRun,
        container: &str,
        from: LogFrom,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.inner.sidecar_log_stream(step, container, from).await
    }

    async fn launch_service(
        &self,
        run: &RunId,
        take: i64,
        name: &str,
        spec: &scarab_pipeline::ServiceSpec,
    ) -> Result<ExecHandle, ExecError> {
        // A shared service is launched with literal `env` only: an empty
        // generation, so its log is ledger-complete.
        let (step, attempt) = crate::logs::service_stream_key(name, take);
        let unit = LogStreamId::new(run, &step, &attempt);
        let now = self.clock.now().await;
        self.logs
            .record_launch_values(&unit, None, Vec::new(), now)
            .await
            .map_err(ledger_launch_error)?;
        let handle = self.inner.launch_service(run, take, name, spec).await?;
        self.logs
            .db()
            .write_log_unit(&unit, Some(&handle.0), 0, None, now)
            .await
            .map_err(|e| ExecError::Other(format!("recording the service's handle: {e}")))?;
        Ok(handle)
    }

    async fn service_ready(&self, handle: &ExecHandle) -> Result<bool, ExecError> {
        self.inner.service_ready(handle).await
    }

    async fn teardown_service(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        self.inner.teardown_service(handle).await
    }

    async fn service_log_stream(
        &self,
        handle: &ExecHandle,
        from: LogFrom,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.inner.service_log_stream(handle, from).await
    }
}
