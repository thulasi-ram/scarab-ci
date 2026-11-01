//! Signals to a server process, sent from the kind node's PID namespace.
//!
//! In its container the server is PID 1 with no init
//! (`docker/server/Dockerfile`), and the kernel drops a signal sent from
//! inside that PID namespace when PID 1 has no handler for it. So the
//! harness does what `crates/scarab-e2e/tests/crash_resume.rs` does: it
//! reads the container's host PID with `crictl inspect` on the node, checks
//! that the PID's cgroup names that container (the Depot runs the same
//! binary, so a process name proves nothing), and runs `kill` there through
//! `docker exec <node>`. ADR-0077's first Amendment says delivery of SIGSTOP
//! this way is not verified, so [`Node::wait_state`] reads
//! `/proc/<pid>/status` to see that it took effect.

use std::time::Duration;

/// The kind node container, from `SCARAB_CHAOS_NODE` (`deploy/kind/chaos.sh`
/// sets it from `deploy/kind/lib.sh`).
pub const DEFAULT_NODE: &str = "scarab-dev-control-plane";

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
}

impl Node {
    pub fn from_env() -> Node {
        Node {
            name: std::env::var("SCARAB_CHAOS_NODE").unwrap_or_else(|_| DEFAULT_NODE.into()),
        }
    }

    /// `docker exec <node> <args>`; its stdout, trimmed.
    pub async fn exec(&self, args: &[&str]) -> Result<String, String> {
        let out = tokio::process::Command::new("docker")
            .arg("exec")
            .arg(&self.name)
            .args(args)
            .output()
            .await
            .map_err(|e| format!("docker exec {}: {e}", self.name))?;
        if !out.status.success() {
            return Err(format!(
                "docker exec {} {args:?}: {}: {}",
                self.name,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// The node-side PID of `container_id`'s process, checked against the
    /// PID's cgroup.
    pub async fn container_pid(&self, container_id: &str) -> Result<i64, String> {
        let raw = self
            .exec(&[
                "crictl",
                "inspect",
                "--output",
                "go-template",
                "--template",
                "{{.info.pid}}",
                container_id,
            ])
            .await?;
        let pid = parse_pid(&raw)
            .ok_or_else(|| format!("crictl gave no PID for container {container_id}: {raw:?}"))?;
        let cgroup = self.exec(&["cat", &format!("/proc/{pid}/cgroup")]).await?;
        if !cgroup.contains(container_id) {
            return Err(format!(
                "PID {pid} is not in container {container_id}'s cgroup: {cgroup}"
            ));
        }
        Ok(pid)
    }

    /// `kill -<signal> <pid>` on the node.
    pub async fn signal(&self, pid: i64, signal: &str) -> Result<(), String> {
        self.exec(&["kill", &format!("-{signal}"), &pid.to_string()])
            .await
            .map(|_| ())
    }

    /// The `State:` line of `/proc/<pid>/status`.
    pub async fn state_line(&self, pid: i64) -> Result<String, String> {
        let status = self.exec(&["cat", &format!("/proc/{pid}/status")]).await?;
        status
            .lines()
            .find(|l| l.starts_with("State:"))
            .map(str::to_string)
            .ok_or_else(|| format!("/proc/{pid}/status has no State: line"))
    }

    /// Poll `/proc/<pid>/status` until it is stopped (`want_stopped`) or not,
    /// for up to `within`. Returns whether it got there, and the last
    /// `State:` line (or the error reading it).
    pub async fn wait_state(
        &self,
        pid: i64,
        want_stopped: bool,
        within: Duration,
    ) -> (bool, String) {
        let deadline = std::time::Instant::now() + within;
        loop {
            let line = self.state_line(pid).await;
            let last = match &line {
                Ok(l) if is_stopped(l) == want_stopped => return (true, l.clone()),
                Ok(l) => l.clone(),
                Err(e) => e.clone(),
            };
            if std::time::Instant::now() > deadline {
                return (false, last);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Node {
    /// `docker <verb> <node>` from the host (`pause`, `unpause`).
    pub async fn docker(&self, verb: &str) -> Result<(), String> {
        let out = tokio::process::Command::new("docker")
            .args([verb, &self.name])
            .output()
            .await
            .map_err(|e| format!("docker {verb} {}: {e}", self.name))?;
        if !out.status.success() {
            return Err(format!(
                "docker {verb} {}: {}: {}",
                self.name,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// `docker inspect`'s `State.Paused` of the node container.
    pub async fn is_paused(&self) -> Result<bool, String> {
        let out = tokio::process::Command::new("docker")
            .args(["inspect", "--format", "{{.State.Paused}}", &self.name])
            .output()
            .await
            .map_err(|e| format!("docker inspect {}: {e}", self.name))?;
        if !out.status.success() {
            return Err(format!(
                "docker inspect {}: {}: {}",
                self.name,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        parse_paused(&String::from_utf8_lossy(&out.stdout))
    }
}

/// `docker inspect --format {{.State.Paused}}`: `true` or `false`.
pub fn parse_paused(raw: &str) -> Result<bool, String> {
    match raw.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("docker inspect gave State.Paused {other:?}")),
    }
}

/// Unpauses the kind node container when dropped, unless disarmed: an error,
/// a panic or an early return between the `docker pause` and the planned
/// `docker unpause` must not leave the node paused. Blocking on purpose:
/// `Drop` cannot await. A signal that kills the process skips `Drop`;
/// `pause-node` unpauses on SIGINT and SIGTERM itself, and `chaos.sh`'s exit
/// trap unpauses a paused node on every path.
pub struct UnpauseGuard {
    node: String,
    armed: bool,
}

/// The node a live [`UnpauseGuard`] may have paused, for the signal handler
/// ([`unpause_on_signal`]), which exits without running `Drop`.
static PAUSED_NODE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn set_paused_node(node: Option<String>) {
    *PAUSED_NODE.lock().unwrap_or_else(|p| p.into_inner()) = node;
}

/// `docker unpause <node>`, blocking, logged.
fn unpause_blocking(node: &str, why: &str) {
    let res = std::process::Command::new("docker").args(["unpause", node]).output();
    eprintln!("scarab-chaos: {why}: docker unpause {node}: {res:?}");
}

impl UnpauseGuard {
    /// Arm before the `docker pause`, so a pause that half-happened is undone
    /// too (an unpause of a running container fails and changes nothing).
    pub fn new(node: &Node) -> Self {
        set_paused_node(Some(node.name.clone()));
        Self {
            node: node.name.clone(),
            armed: true,
        }
    }

    /// The node is unpaused (checked): nothing left to undo.
    pub fn disarm(&mut self) {
        self.armed = false;
        set_paused_node(None);
    }
}

impl Drop for UnpauseGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        set_paused_node(None);
        unpause_blocking(&self.node, "cleanup");
    }
}

/// On SIGINT or SIGTERM: unpause the node a live [`UnpauseGuard`] holds, then
/// exit (130 or 143, as a shell reports those signals). Installed for the
/// whole `pause-node` run: once tokio handles a signal its default action is
/// gone for the life of the process, so the handler also has to exit.
pub fn unpause_on_signal() -> Result<(), String> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut int = signal(SignalKind::interrupt()).map_err(|e| format!("SIGINT handler: {e}"))?;
    let mut term = signal(SignalKind::terminate()).map_err(|e| format!("SIGTERM handler: {e}"))?;
    tokio::spawn(async move {
        let code = tokio::select! {
            _ = int.recv() => 130,
            _ = term.recv() => 143,
        };
        let node = PAUSED_NODE.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(node) = node {
            unpause_blocking(&node, "signal");
        }
        std::process::exit(code);
    });
    Ok(())
}

/// A `crictl` PID: digits, not 0.
pub fn parse_pid(raw: &str) -> Option<i64> {
    let t = raw.trim();
    if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    t.parse().ok().filter(|p| *p > 0)
}

/// `State:\tT (stopped)`: the process is stopped by a signal.
pub fn is_stopped(state_line: &str) -> bool {
    state_line
        .strip_prefix("State:")
        .is_some_and(|rest| rest.trim_start().starts_with('T'))
}

/// Sends SIGCONT to a stopped process when dropped, unless disarmed: an
/// error or a panic between the SIGSTOP and the planned SIGCONT must not
/// leave a server stopped. Blocking on purpose: `Drop` cannot await.
pub struct ContGuard {
    node: String,
    pid: i64,
    armed: bool,
}

impl ContGuard {
    pub fn new(node: &Node, pid: i64) -> Self {
        Self {
            node: node.name.clone(),
            pid,
            armed: true,
        }
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ContGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let res = std::process::Command::new("docker")
            .args(["exec", &self.node, "kill", "-CONT", &self.pid.to_string()])
            .output();
        eprintln!(
            "scarab-chaos: cleanup: SIGCONT to PID {} on {}: {res:?}",
            self.pid, self.node
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_stopped_state_line() {
        assert!(is_stopped("State:\tT (stopped)"));
        assert!(!is_stopped("State:\tS (sleeping)"));
        assert!(!is_stopped("State:\tR (running)"));
        // `t` is a ptrace stop, not SIGSTOP.
        assert!(!is_stopped("State:\tt (tracing stop)"));
        assert!(!is_stopped("Name:\tT"));
    }

    #[test]
    fn reads_the_paused_state() {
        assert_eq!(parse_paused("true\n"), Ok(true));
        assert_eq!(parse_paused("false"), Ok(false));
        assert!(parse_paused("").is_err());
    }

    #[test]
    fn a_crictl_pid_is_digits_and_not_zero() {
        assert_eq!(parse_pid("4242\n"), Some(4242));
        assert_eq!(parse_pid("0"), None);
        assert_eq!(parse_pid(""), None);
        assert_eq!(parse_pid("<no value>"), None);
    }
}
