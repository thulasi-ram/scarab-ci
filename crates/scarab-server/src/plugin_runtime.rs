//! The plugin runtime's reconcile loop (ADR-0072 §9): every
//! [`RECONCILE_INTERVAL`], on the one replica holding the
//! [`PLUGIN_RUNTIME_LEASE`], make the cluster match the PluginInstallation
//! registry.
//!
//! One pass:
//!
//! 1. **Baseline.** The plugin namespace must enforce `restricted` Pod
//!    Security and hold only the runtime's own NetworkPolicies (applied
//!    here). Refused → every workload is stopped and every enabled
//!    installation shows `error` with the reason. Fail closed.
//! 2. **Isolation probe**, when due: at manager boot (the first pass after
//!    this replica takes the lease), every [`PROBE_MAX_AGE_MS`], whenever an
//!    enabled installation has not been probed for (a fresh install, or an
//!    enable — the store forgets the last probe then), and every
//!    [`PROBE_FAILED_RETRY_MS`] while the last one failed. A failed probe
//!    stops every workload and marks enabled installations `probe_failed`:
//!    nothing runs on a cluster that does not enforce the policy.
//! 3. **Each installation.** Killed → stopped (scaled to zero, its
//!    ServiceAccount — and with it every token — deleted). Enabled with a
//!    passed probe → applied, and its status read back from the cluster.
//! 4. **Orphans.** A workload whose installation is gone (uninstalled) is
//!    deleted.
//!
//! The kill switch does not wait for this loop: the broker reads the
//! installation's state on every request. The loop is what takes the
//! workload down and revokes its tokens at the source.
//!
//! What to do with an installation is decided by pure functions
//! ([`probe_due`], [`decide`]); the cluster calls are the adapter's.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use scarab_engine::{Clock, Db};
use scarab_plugin::{InstallationState, PluginInstallation, PluginStore, RuntimeStatus};
use scarab_plugin_k8s::{K8sPluginRuntime, WorkloadState};

/// The lease gating the loop to one replica at a time.
pub const PLUGIN_RUNTIME_LEASE: &str = "plugin-runtime";
/// Long enough to outlast a pass that runs a probe (a few minutes at worst
/// on a cold node), so a second replica never probes concurrently.
const PLUGIN_RUNTIME_LEASE_TTL_MS: i64 = 5 * 60 * 1000;
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(15);
/// A passed probe is trusted this long; a CNI can change under a running
/// cluster.
pub const PROBE_MAX_AGE_MS: i64 = 6 * 60 * 60 * 1000;
/// How soon a FAILED probe is tried again — without an operator action. An
/// enable resets the installation, which probes at once.
pub const PROBE_FAILED_RETRY_MS: i64 = 5 * 60 * 1000;
/// `runtime_message` is shown in the UI; a Kubernetes error can be long.
const MAX_RUNTIME_MESSAGE_CHARS: usize = 1024;

/// What this replica last learned from the isolation probe. Reset whenever
/// it does not hold the lease, so taking the lease is a "manager boot".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Isolation {
    /// When the last probe finished (Unix-ms); `None` = none since boot.
    pub probed_at: Option<i64>,
    /// `None` = passed; `Some(why)` = failed.
    pub failure: Option<String>,
}

/// Whether this pass must run the isolation probe.
pub fn probe_due(now: i64, isolation: &Isolation, installations: &[PluginInstallation]) -> bool {
    let enabled: Vec<&PluginInstallation> = installations
        .iter()
        .filter(|i| i.state == InstallationState::Enabled)
        .collect();
    if enabled.is_empty() {
        // Nothing would start: no Pods are created to prove nothing.
        return false;
    }
    let Some(probed_at) = isolation.probed_at else {
        return true; // manager boot
    };
    let age = now.saturating_sub(probed_at);
    if age >= PROBE_MAX_AGE_MS {
        return true;
    }
    // A fresh install or an enable (the store forgets the last probe then)
    // has never been probed for: probe now, whatever the last result. One
    // that last failed its probe is not fresh — it waits for the retry
    // below, so a namespace that keeps failing is not probed every 15 s.
    let fresh = enabled
        .iter()
        .any(|i| i.probe_passed_at.is_none() && i.runtime_status != RuntimeStatus::ProbeFailed);
    if fresh {
        return true;
    }
    isolation.failure.is_some() && age >= PROBE_FAILED_RETRY_MS
}

/// What a pass does with one installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Scale to zero and delete its ServiceAccount; record `stopped`.
    Stop,
    /// Not allowed to run: stop it, and record why.
    Hold {
        status: RuntimeStatus,
        message: String,
    },
    /// Apply its workload; `probe_passed_at` is the probe that allowed it.
    Run { probe_passed_at: i64 },
    /// No probe result yet (none was due — nothing enabled at the time):
    /// leave it as it is.
    Wait,
}

/// Decide one installation from the namespace baseline and the isolation
/// result. Killed always stops; nothing enabled runs without both a
/// baseline and a passed probe.
pub fn decide(
    installation: &PluginInstallation,
    baseline: &Result<(), String>,
    isolation: &Isolation,
) -> Decision {
    if installation.state == InstallationState::Killed {
        return Decision::Stop;
    }
    if let Err(why) = baseline {
        return Decision::Hold {
            status: RuntimeStatus::Error,
            message: why.clone(),
        };
    }
    match (&isolation.failure, isolation.probed_at) {
        (Some(why), _) => Decision::Hold {
            status: RuntimeStatus::ProbeFailed,
            message: why.clone(),
        },
        (None, Some(at)) => Decision::Run {
            probe_passed_at: at,
        },
        (None, None) => Decision::Wait,
    }
}

/// The status an applied workload is recorded with.
fn running_status(state: &WorkloadState) -> (RuntimeStatus, String) {
    match state {
        WorkloadState::Available => (RuntimeStatus::Running, String::new()),
        WorkloadState::Starting(what) => (RuntimeStatus::Pending, format!("starting: {what}")),
        WorkloadState::Failing(why) => (RuntimeStatus::Error, why.clone()),
        // Just applied: the cluster has not caught up yet.
        WorkloadState::Absent | WorkloadState::Stopped => {
            (RuntimeStatus::Pending, "starting".to_string())
        }
    }
}

fn capped(message: &str) -> String {
    if message.chars().count() <= MAX_RUNTIME_MESSAGE_CHARS {
        message.to_string()
    } else {
        let mut s: String = message
            .chars()
            .take(MAX_RUNTIME_MESSAGE_CHARS - 1)
            .collect();
        s.push('…');
        s
    }
}

/// The loop's state and handles.
pub struct PluginRuntimeLoop {
    db: Arc<dyn Db>,
    store: Arc<dyn PluginStore>,
    runtime: Arc<K8sPluginRuntime>,
    clock: Arc<dyn Clock>,
    owner: String,
    isolation: Isolation,
}

impl PluginRuntimeLoop {
    pub fn new(
        db: Arc<dyn Db>,
        store: Arc<dyn PluginStore>,
        runtime: Arc<K8sPluginRuntime>,
        clock: Arc<dyn Clock>,
        owner: String,
    ) -> Self {
        Self {
            db,
            store,
            runtime,
            clock,
            owner,
            isolation: Isolation::default(),
        }
    }

    /// Run a pass every [`RECONCILE_INTERVAL`], forever.
    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.reconcile_once().await {
                    tracing::warn!(error = %e, "plugin runtime: reconcile pass failed");
                }
                tokio::time::sleep(RECONCILE_INTERVAL).await;
            }
        })
    }

    /// One pass. A replica that does not hold the lease does nothing, and
    /// forgets its probe result.
    pub async fn reconcile_once(&mut self) -> Result<(), String> {
        let lease = self
            .db
            .lease(
                PLUGIN_RUNTIME_LEASE,
                &self.owner,
                PLUGIN_RUNTIME_LEASE_TTL_MS,
            )
            .await
            .map_err(|e| e.to_string())?;
        if lease.owner != self.owner {
            self.isolation = Isolation::default();
            return Ok(());
        }
        let installations = self
            .store
            .all_installations()
            .await
            .map_err(|e| e.to_string())?;

        let baseline = self
            .runtime
            .ensure_baseline()
            .await
            .map_err(|e| e.to_string());
        if let Err(why) = &baseline {
            tracing::error!(reason = %why, "plugin runtime: namespace baseline refused; no plugin runs");
            // Nothing proven about the namespace any more.
            self.isolation = Isolation::default();
        } else {
            let now = self.clock.now().await.0;
            if probe_due(now, &self.isolation, &installations) {
                self.probe(&installations).await;
            }
        }

        for installation in &installations {
            if let Err(e) = self.reconcile_installation(installation, &baseline).await {
                tracing::warn!(
                    installation = %installation.id,
                    org = %installation.org,
                    name = %installation.name,
                    error = %e,
                    "plugin runtime: reconcile failed for an installation"
                );
            }
        }

        // Only after a successful registry read: an id missing from a FAILED
        // read is not an uninstall.
        let known: BTreeSet<&str> = installations.iter().map(|i| i.id.as_str()).collect();
        for orphan in self
            .runtime
            .workload_ids()
            .await
            .map_err(|e| e.to_string())?
        {
            if !known.contains(orphan.as_str()) {
                tracing::info!(installation = %orphan, "plugin runtime: removing an uninstalled installation's workload");
                self.runtime
                    .uninstall(&orphan)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    async fn probe(&mut self, installations: &[PluginInstallation]) {
        // Shown while it runs, for what is waiting on it.
        for i in installations.iter().filter(|i| {
            i.state == InstallationState::Enabled && i.runtime_status == RuntimeStatus::Pending
        }) {
            let _ = self
                .store
                .set_runtime_status(
                    &i.id,
                    InstallationState::Enabled,
                    RuntimeStatus::Probing,
                    "isolation probe running",
                    i.probe_passed_at,
                )
                .await;
        }
        let report = self.runtime.probe_isolation().await;
        let finished = self.clock.now().await.0;
        if report.passed() {
            tracing::info!("plugin runtime: isolation probe passed");
        } else {
            tracing::error!(
                findings = %report.message(),
                "plugin runtime: isolation probe FAILED; no plugin runs"
            );
        }
        self.isolation = Isolation {
            probed_at: Some(finished),
            failure: (!report.passed()).then(|| report.message()),
        };
    }

    async fn reconcile_installation(
        &self,
        installation: &PluginInstallation,
        baseline: &Result<(), String>,
    ) -> Result<(), String> {
        let (status, message, probe_passed_at) =
            match decide(installation, baseline, &self.isolation) {
                Decision::Wait => return Ok(()),
                Decision::Stop => {
                    self.runtime
                        .stop(&installation.id)
                        .await
                        .map_err(|e| e.to_string())?;
                    (
                        RuntimeStatus::Stopped,
                        String::new(),
                        installation.probe_passed_at,
                    )
                }
                Decision::Hold { status, message } => {
                    self.runtime
                        .stop(&installation.id)
                        .await
                        .map_err(|e| e.to_string())?;
                    (status, message, None)
                }
                Decision::Run { probe_passed_at } => {
                    let applied = self.runtime.ensure(installation).await;
                    let (status, message) = match applied {
                        Ok(()) => running_status(
                            &self
                                .runtime
                                .workload_state(&installation.id)
                                .await
                                .map_err(|e| e.to_string())?,
                        ),
                        Err(e) => (RuntimeStatus::Error, e.to_string()),
                    };
                    (status, message, Some(probe_passed_at))
                }
            };
        let message = capped(&message);
        let unchanged = installation.runtime_status == status
            && installation.runtime_message == message
            && installation.probe_passed_at == probe_passed_at;
        if !unchanged {
            self.store
                .set_runtime_status(
                    &installation.id,
                    installation.state,
                    status,
                    &message,
                    probe_passed_at,
                )
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use scarab_plugin::{PluginCapability, PluginManifest};

    const HOUR_MS: i64 = 60 * 60 * 1000;

    fn installation(state: InstallationState) -> PluginInstallation {
        PluginInstallation {
            id: "0123456789".into(),
            org: "acme".into(),
            name: "argocd".into(),
            plugin: "argocd".into(),
            image: format!("ghcr.io/acme/argocd@sha256:{}", "a".repeat(64)),
            manifest: PluginManifest {
                plugin: "argocd".into(),
                capabilities: vec![PluginCapability::ReportDeployments],
                connections: vec![],
                config_keys: vec![],
            },
            manifest_sha256: "0".repeat(64),
            config: BTreeMap::new(),
            state,
            runtime_status: RuntimeStatus::Pending,
            runtime_message: String::new(),
            probe_passed_at: None,
            approved_by: "amy".into(),
            approved_at: 1,
            killed_by: None,
            killed_at: None,
        }
    }

    fn running(probed_at: i64) -> PluginInstallation {
        PluginInstallation {
            runtime_status: RuntimeStatus::Running,
            probe_passed_at: Some(probed_at),
            ..installation(InstallationState::Enabled)
        }
    }

    fn passed(at: i64) -> Isolation {
        Isolation {
            probed_at: Some(at),
            failure: None,
        }
    }

    fn failed(at: i64) -> Isolation {
        Isolation {
            probed_at: Some(at),
            failure: Some("isolation probe failed: NetworkPolicy is NOT enforced".into()),
        }
    }

    #[test]
    fn the_probe_runs_at_boot_on_install_and_every_six_hours() {
        let now = 100 * HOUR_MS;
        // Manager boot.
        assert!(probe_due(now, &Isolation::default(), &[running(1)]));
        // Fresh install / enable: never probed for, even right after a pass.
        assert!(probe_due(
            now,
            &passed(now - 1000),
            &[
                running(now - 1000),
                installation(InstallationState::Enabled)
            ]
        ));
        // Six hours.
        assert!(!probe_due(now, &passed(now - 5 * HOUR_MS), &[running(1)]));
        assert!(probe_due(now, &passed(now - 6 * HOUR_MS), &[running(1)]));
    }

    #[test]
    fn nothing_enabled_means_no_probe_pods() {
        assert!(!probe_due(1, &Isolation::default(), &[]));
        assert!(!probe_due(
            1,
            &Isolation::default(),
            &[installation(InstallationState::Killed)]
        ));
    }

    #[test]
    fn a_failing_namespace_is_retried_on_a_backoff_not_every_pass() {
        let now = 100 * HOUR_MS;
        let held = PluginInstallation {
            runtime_status: RuntimeStatus::ProbeFailed,
            ..installation(InstallationState::Enabled)
        };
        assert!(!probe_due(
            now,
            &failed(now - 60_000),
            std::slice::from_ref(&held)
        ));
        assert!(probe_due(
            now,
            &failed(now - PROBE_FAILED_RETRY_MS),
            std::slice::from_ref(&held)
        ));
        // Re-enabling (the store resets it to pending, no probe) does not
        // wait for the retry.
        assert!(probe_due(
            now,
            &failed(now - 60_000),
            &[held, installation(InstallationState::Enabled)]
        ));
    }

    #[test]
    fn killed_always_stops() {
        for isolation in [passed(1), failed(1), Isolation::default()] {
            assert_eq!(
                decide(
                    &installation(InstallationState::Killed),
                    &Ok(()),
                    &isolation
                ),
                Decision::Stop
            );
        }
        assert_eq!(
            decide(
                &installation(InstallationState::Killed),
                &Err("refused".into()),
                &passed(1)
            ),
            Decision::Stop
        );
    }

    #[test]
    fn nothing_runs_without_a_baseline_and_a_passed_probe() {
        let enabled = installation(InstallationState::Enabled);
        assert_eq!(
            decide(&enabled, &Err("not restricted".into()), &passed(1)),
            Decision::Hold {
                status: RuntimeStatus::Error,
                message: "not restricted".into()
            }
        );
        assert!(matches!(
            decide(&enabled, &Ok(()), &failed(1)),
            Decision::Hold {
                status: RuntimeStatus::ProbeFailed,
                ..
            }
        ));
        assert_eq!(
            decide(&enabled, &Ok(()), &Isolation::default()),
            Decision::Wait
        );
        assert_eq!(
            decide(&enabled, &Ok(()), &passed(7)),
            Decision::Run { probe_passed_at: 7 }
        );
    }

    #[test]
    fn a_long_kubernetes_error_is_capped_for_the_ui() {
        let long = "x".repeat(5000);
        assert_eq!(capped(&long).chars().count(), MAX_RUNTIME_MESSAGE_CHARS);
        assert_eq!(capped("short"), "short");
    }
}
