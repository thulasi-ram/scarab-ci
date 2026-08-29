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

use scarab_engine::ports::{ExecHandle, ExecState, FailureClass};
use scarab_engine::{ExecError, Executor, StepRun, StepSpec};

/// A tracked child: still running (with its ADR-0047 kill-timer deadline), or
/// finished with an observed state.
enum Proc {
    Running {
        child: Child,
        /// The kill-timer deadline (ADR-0047): once passed, `poll` kills the
        /// child and reports a `Timeout` failure — the local mirror of the
        /// kubelet's `activeDeadlineSeconds`.
        deadline: std::time::Instant,
    },
    Done(ExecState),
}

/// The global default step deadline in seconds (ADR-0047).
pub const DEFAULT_STEP_TIMEOUT_SECS: u32 = 3_600;

/// A local-process executor. Steps run as OS child processes; launched children
/// are tracked by their fence handle so a relaunch re-attaches (idempotency).
pub struct LocalExecutor {
    procs: Mutex<HashMap<String, Proc>>,
    /// Global default step deadline (ADR-0047), when a step declares none.
    default_step_timeout_secs: u32,
}

impl LocalExecutor {
    pub fn new() -> Self {
        Self {
            procs: Mutex::new(HashMap::new()),
            default_step_timeout_secs: DEFAULT_STEP_TIMEOUT_SECS,
        }
    }

    /// Override the global default step deadline (ADR-0047).
    pub fn with_default_step_timeout_secs(mut self, secs: u32) -> Self {
        self.default_step_timeout_secs = secs;
        self
    }

    /// The deterministic handle a step's fence `{run, step, attempt}` maps to — a
    /// pure function of the fence, so the same step always maps to the same handle
    /// (what makes `launch` re-attach rather than relaunch).
    pub fn handle_for(step: &StepRun) -> ExecHandle {
        let attempt = step
            .current_attempt()
            .map(|a| a.id.0.as_str())
            .unwrap_or("0");
        ExecHandle(format!(
            "local://{}/{}/{}",
            step.run.0, step.step.0, attempt
        ))
    }

    /// The per-step working directory for a handle — a pure function of the fence
    /// (the run is the first segment of `local://<run>/<step>/<attempt>`), so
    /// `results` can find where `launch` ran the step.
    fn workdir_for(handle: &ExecHandle) -> Option<std::path::PathBuf> {
        let run = handle.0.strip_prefix("local://")?.split('/').next()?;
        Some(
            std::env::temp_dir()
                .join("scarab-local")
                .join(sanitize(run))
                .join(sanitize(&handle.0)),
        )
    }

    /// The results directory inside a step's workdir (ADR-0008/0041): the step
    /// writes `<name>.json` files here and the orchestrator reads them back.
    fn results_dir(workdir: &std::path::Path) -> std::path::PathBuf {
        workdir.join("scarab").join("results")
    }
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a finished process's exit status to an [`ExecState`], classifying the
/// failure (ADR-0047): a non-zero exit code is the step's own verdict (`Step`);
/// a signal kill (`code() == None` on unix) means the platform — not the step's
/// logic — ended a *started* process, so it is post-start `Infra`.
fn state_of(status: std::process::ExitStatus) -> ExecState {
    if status.success() {
        ExecState::Succeeded
    } else {
        let exit_code = status.code();
        let class = match exit_code {
            Some(_) => FailureClass::Step,
            None => FailureClass::Infra {
                never_started: false,
            },
        };
        ExecState::Failed {
            exit_code,
            class,
            cause: None,
        }
    }
}

#[async_trait]
impl Executor for LocalExecutor {
    async fn infra_condition(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        // The local backend runs the step as a child process: there is no
        // scheduler to reject it, no image to pull, and no node to be too small
        // — so it has no infra plane to narrate. REQUIRED method, so this None
        // is a decision the compiler saw.
        Ok(None)
    }

    async fn workspace_provisioning(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        // The local backend has no fan-in sensor: it has no workspace CAS leg
        // at all (parity explicitly deferred), so there is nothing to report.
        // REQUIRED method, so this None is a decision the compiler saw.
        Ok(None)
    }

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

        // Clone steps run the canonical scarab-clone image with tmpfs token
        // delivery (ADR-0045) — a Pod-shaped contract the host-process backend
        // cannot honor. Fail with direction, never a silent no-source run.
        if spec.clone.is_some() {
            return Err(ExecError::Launch(
                "clone steps require the k8s executor (scarab-clone image + tmpfs \
                 credential delivery, ADR-0045); the local backend has no clone support"
                    .into(),
            ));
        }
        // Same contract for build steps (ADR-0018): rootless BuildKit is a
        // Pod-shaped capability the host-process backend cannot honor.
        if spec.build.is_some() {
            return Err(ExecError::Launch(
                "build steps require the k8s executor (rootless BuildKit, ADR-0018); \
                 the local backend has no image-build support"
                    .into(),
            ));
        }
        // Sidecar services (ADR-0058) co-locate an author-supplied container image
        // in the step's Pod (localhost-reachable, restricted baseline). The
        // host-process backend runs a bare command, not a container image, so it
        // cannot honor a sidecar — fail with direction, never a silent no-service
        // run. Same Pod-shaped contract as clone/build above.
        if !spec.services.is_empty() {
            return Err(ExecError::Launch(
                "sidecar services require the k8s executor (a co-located container in \
                 the step's Pod, ADR-0058); the local backend runs bare host processes \
                 and has no sidecar support"
                    .into(),
            ));
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
        // Results channel (ADR-0008/0041): the step writes `<name>.json` files
        // under `$SCARAB_RESULTS`, read back on success as named results.
        let results_dir = Self::results_dir(&workdir);
        std::fs::create_dir_all(&results_dir)
            .map_err(|e| ExecError::Launch(format!("results dir: {e}")))?;

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
                step.current_attempt()
                    .map(|a| a.id.0.as_str())
                    .unwrap_or("0"),
            )
            .env("SCARAB_RESULTS", &results_dir);

        let child = cmd
            .spawn()
            .map_err(|e| ExecError::Launch(format!("spawn `{program}`: {e}")))?;

        // Kill-timer deadline (ADR-0047): the step's authored timeout or the
        // configured global default.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(
                spec.timeout_seconds
                    .unwrap_or(self.default_step_timeout_secs) as u64,
            );
        self.procs
            .lock()
            .unwrap()
            .insert(handle.0.clone(), Proc::Running { child, deadline });
        Ok(handle)
    }

    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
        let mut procs = self.procs.lock().unwrap();
        match procs.get_mut(&handle.0) {
            // Never launched here / lost across a restart — the orchestrator relaunches.
            None => Ok(ExecState::Lost),
            Some(Proc::Done(state)) => Ok(state.clone()),
            Some(slot @ Proc::Running { .. }) => {
                let Proc::Running { child, deadline } = slot else {
                    unreachable!()
                };
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let state = state_of(status);
                        *slot = Proc::Done(state.clone());
                        Ok(state)
                    }
                    // Kill-timer (ADR-0047): past the deadline, kill the child
                    // and report a classified Timeout — the local mirror of
                    // the kubelet's activeDeadlineSeconds.
                    Ok(None) if std::time::Instant::now() >= *deadline => {
                        let _ = child.start_kill();
                        let state = ExecState::Failed {
                            exit_code: None,
                            class: FailureClass::Timeout,
                            cause: None,
                        };
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
        if let Some(Proc::Running { child, .. }) = procs.get_mut(&handle.0) {
            // Best-effort kill; `kill_on_drop` also reaps if the slot is dropped.
            // A cancel is the platform ending a started process (ADR-0047).
            let _ = child.start_kill();
            procs.insert(
                handle.0.clone(),
                Proc::Done(ExecState::Failed {
                    exit_code: None,
                    class: FailureClass::Infra {
                        never_started: false,
                    },
                    cause: None,
                }),
            );
        }
        Ok(())
    }

    // `output` uses the port default (`None`): no workspace CAS locally (ADR-0036).


    /// Read the step's named results (ADR-0041) from its results dir: every
    /// `<name>.json` file becomes result `<name>` with the file's parsed JSON
    /// value. An absent dir (the step emitted nothing) yields an empty map; a
    /// malformed result file fails fast (surfaces rather than being swallowed).
    async fn results(
        &self,
        handle: &ExecHandle,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ExecError> {
        let mut out = std::collections::BTreeMap::new();
        let Some(workdir) = Self::workdir_for(handle) else {
            return Ok(out);
        };
        let dir = Self::results_dir(&workdir);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Ok(out), // no results emitted
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let bytes = std::fs::read(&path)
                .map_err(|e| ExecError::Other(format!("read result `{name}`: {e}")))?;
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| ExecError::Other(format!("parse result `{name}`: {e}")))?;
            out.insert(name.to_string(), value);
        }
        Ok(out)
    }
}

/// Sanitize a fence component into a filesystem-safe path segment.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
