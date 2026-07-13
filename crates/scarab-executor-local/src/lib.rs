//! Local-process adapter for the [`scarab_engine::Executor`] port.
//!
//! A **non-production** developer/CLI/test backend (ADR-0036): each step runs as
//! an OS child process via `tokio`'s process support — no Docker, no Kubernetes.
//! Its purpose is a fast laptop inner loop (`scarab run`) and driving the real
//! engine end-to-end in tests without a cluster. Production execution is
//! Kubernetes-only (ADR-0005); this adapter is never a deployment backend.
//!
//! Semantics (deliberately narrower than the k8s adapter, per ADR-0036):
//! - **Idempotent on the fence *within a process*.** Relaunching the same
//!   `{run, step, attempt}` re-attaches to the tracked child rather than spawning
//!   a second one. Surviving a control-plane *restart* is the k8s adapter's job:
//!   after a local restart the child is untracked, so `poll` reports `Lost` and
//!   the orchestrator relaunches (ADR-0020).
//! - **No content-addressed workspace.** `output` returns `None`; workspace CAS
//!   is the k8s post-step path. Each step runs in its own temp working dir.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::process::{Child, Command};

use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{ExecError, Executor, StepRun, StepSpec};

/// A tracked child: still running, or finished with an observed state.
enum Proc {
    Running(Child),
    Done(ExecState),
}

/// A local-process executor. Steps run as OS child processes; launched children
/// are tracked by their fence handle so a relaunch re-attaches (idempotency).
pub struct LocalExecutor {
    procs: Mutex<HashMap<String, Proc>>,
}

impl LocalExecutor {
    pub fn new() -> Self {
        Self {
            procs: Mutex::new(HashMap::new()),
        }
    }

    /// The deterministic handle a step's fence `{run, step, attempt}` maps to — a
    /// pure function of the fence, so the same step always maps to the same handle
    /// (what makes `launch` re-attach rather than relaunch).
    pub fn handle_for(step: &StepRun) -> ExecHandle {
        let attempt = step
            .current_attempt()
            .map(|a| a.id.0.as_str())
            .unwrap_or("0");
        ExecHandle(format!("local://{}/{}/{}", step.run.0, step.step.0, attempt))
    }
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a finished process's exit status to an [`ExecState`].
fn state_of(status: std::process::ExitStatus) -> ExecState {
    if status.success() {
        ExecState::Succeeded
    } else {
        ExecState::Failed {
            exit_code: status.code(),
        }
    }
}

#[async_trait]
impl Executor for LocalExecutor {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        let handle = Self::handle_for(step);

        // Re-attach: if this fence already has a tracked child (a prior launch we
        // may not have observed completing), adopt it instead of spawning again.
        {
            let procs = self.procs.lock().unwrap();
            if procs.contains_key(&handle.0) {
                return Ok(handle);
            }
        }

        // The step contract is an OCI image + command; locally there is no image,
        // so a command is required (ADR-0036).
        let (program, args) = spec
            .command
            .split_first()
            .ok_or_else(|| ExecError::Launch("local executor requires a command".into()))?;

        // Each step gets its own temp working dir (no shared workspace in v1).
        let workdir = std::env::temp_dir()
            .join("scarab-local")
            .join(sanitize(&step.run.0))
            .join(sanitize(&handle.0));
        std::fs::create_dir_all(&workdir)
            .map_err(|e| ExecError::Launch(format!("workdir: {e}")))?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(&workdir)
            .kill_on_drop(true)
            .env_clear()
            .envs(std::env::vars()) // inherit PATH etc. for a usable dev shell
            .envs(spec.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        // Fence env vars (cooperating idempotency, ADR-0021), mirroring k8s.
        cmd.env("SCARAB_RUN", &step.run.0)
            .env("SCARAB_STEP", &step.step.0)
            .env(
                "SCARAB_ATTEMPT",
                step.current_attempt().map(|a| a.id.0.as_str()).unwrap_or("0"),
            );

        let child = cmd
            .spawn()
            .map_err(|e| ExecError::Launch(format!("spawn `{program}`: {e}")))?;

        self.procs
            .lock()
            .unwrap()
            .insert(handle.0.clone(), Proc::Running(child));
        Ok(handle)
    }

    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
        let mut procs = self.procs.lock().unwrap();
        match procs.get_mut(&handle.0) {
            // Never launched here / lost across a restart — the orchestrator relaunches.
            None => Ok(ExecState::Lost),
            Some(Proc::Done(state)) => Ok(state.clone()),
            Some(slot @ Proc::Running(_)) => {
                let Proc::Running(child) = slot else {
                    unreachable!()
                };
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let state = state_of(status);
                        *slot = Proc::Done(state.clone());
                        Ok(state)
                    }
                    Ok(None) => Ok(ExecState::Running),
                    Err(e) => Err(ExecError::Other(format!("try_wait: {e}"))),
                }
            }
        }
    }

    async fn cancel(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        let mut procs = self.procs.lock().unwrap();
        if let Some(Proc::Running(child)) = procs.get_mut(&handle.0) {
            // Best-effort kill; `kill_on_drop` also reaps if the slot is dropped.
            let _ = child.start_kill();
            procs.insert(
                handle.0.clone(),
                Proc::Done(ExecState::Failed { exit_code: None }),
            );
        }
        Ok(())
    }

    // `output` uses the port default (`None`): no workspace CAS locally (ADR-0036).
}

/// Sanitize a fence component into a filesystem-safe path segment.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}
