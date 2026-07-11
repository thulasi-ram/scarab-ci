//! Local-process adapter for the [`scarab_engine::Executor`] port.
//!
//! Adapter crate: pairs the pure `scarab-engine` domain with `tokio`'s process
//! support (no Docker/bollard dependency needed). Runs steps as child
//! processes on the host — handy for `scarab-cli` and dev loops. Stub.

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{ExecError, Executor, StepRun, StepSpec};

/// A local-process executor. Steps run as OS child processes under `shell`.
pub struct LocalExecutor {
    #[allow(dead_code)]
    shell: String,
}

impl LocalExecutor {
    pub fn new() -> Self {
        Self {
            shell: "/bin/sh".to_string(),
        }
    }
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for LocalExecutor {
    async fn launch(&self, _step: &StepRun, _spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        // TODO: spawn `self.shell -c <script>` and track the child pid.
        let _cmd = tokio::process::Command::new(&self.shell);
        unimplemented!("LocalExecutor::launch")
    }

    async fn poll(&self, _handle: &ExecHandle) -> Result<ExecState, ExecError> {
        unimplemented!("LocalExecutor::poll")
    }

    async fn cancel(&self, _handle: &ExecHandle) -> Result<(), ExecError> {
        unimplemented!("LocalExecutor::cancel")
    }
}
