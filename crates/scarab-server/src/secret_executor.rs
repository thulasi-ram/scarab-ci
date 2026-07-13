//! `SecretInjectingExecutor` — a launch-time secret-injection decorator (ADR-0037).
//!
//! Secret resolution needs the [`SecretProvider`] *and* the [`LogService`]
//! redactor (both server-layer), but injection must land in the Pod's env, which
//! happens in the executor at launch — across the ADR-0004 outbox boundary where
//! only ids travel. This decorator bridges that: it wraps the real [`Executor`],
//! and just before `launch` it resolves the step's declared secret keys against
//! the run's deploy-context scope (`env → repo → org` inheritance), registers the
//! values with the redactor, and merges them into `spec.env`. Resolved values are
//! never persisted — they exist only in the launch call.
//!
//! A fork-PR run (its deploy context's `locked_out` flag) receives no secrets. A
//! run without a deploy context (ordinary CI) resolves nothing here.

use std::sync::Arc;

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, Executor};
use scarab_engine::{Db, ExecError, StepRun, StepSpec};
use scarab_secrets::{SecretProvider, SecretScope};

use crate::{resolve_step_secrets, LogService};

/// Wraps an [`Executor`], injecting a step's declared secrets into its env at
/// launch time.
pub struct SecretInjectingExecutor {
    inner: Arc<dyn Executor>,
    db: Arc<dyn Db>,
    secrets: Arc<dyn SecretProvider>,
    logs: Arc<LogService>,
}

impl SecretInjectingExecutor {
    pub fn new(
        inner: Arc<dyn Executor>,
        db: Arc<dyn Db>,
        secrets: Arc<dyn SecretProvider>,
        logs: Arc<LogService>,
    ) -> Self {
        Self {
            inner,
            db,
            secrets,
            logs,
        }
    }
}

#[async_trait]
impl Executor for SecretInjectingExecutor {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        // No declared secrets → nothing to inject.
        if spec.secrets.is_empty() {
            return self.inner.launch(step, spec).await;
        }
        // Env-scoped secrets resolve against the run's deploy context. A run
        // without one (ordinary CI) has no scope here, so it gets none.
        let ctx = self
            .db
            .run_deploy_context(&step.run)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?;
        let Some(ctx) = ctx else {
            return self.inner.launch(step, spec).await;
        };
        let scope = SecretScope::Environment {
            org: ctx.org,
            repo: ctx.repo,
            environment: ctx.environment,
        };
        // Resolve (with inheritance), register each value with the redactor, and
        // honor the fork-PR lockout — all inside `resolve_step_secrets`.
        let resolved = resolve_step_secrets(
            self.secrets.as_ref(),
            &self.logs,
            &scope,
            &spec.secrets,
            ctx.locked_out,
        )
        .await
        .map_err(|e| ExecError::Other(e.to_string()))?;

        let mut spec = spec.clone();
        spec.env.extend(resolved);
        self.inner.launch(step, &spec).await
    }

    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
        self.inner.poll(handle).await
    }

    async fn cancel(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        self.inner.cancel(handle).await
    }

    async fn output(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        self.inner.output(handle).await
    }
}
