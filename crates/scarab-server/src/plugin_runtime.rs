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
//!    installation shows `error` with the reason. Fail closed. A Kubernetes
//!    error while checking (a timeout, a 5xx, an admission-webhook blip) is
//!    NOT a refusal: it proves nothing either way, so the pass is skipped —
//!    running workloads stay up, the probe result is kept — and only Killed
//!    installations are still stopped.
//! 2. **Isolation probe**, when due: at manager boot (the first pass after
//!    this replica takes the lease), every [`PROBE_MAX_AGE_MS`], whenever an
//!    enabled installation has not been probed for (a fresh install, or an
//!    enable — the store forgets the last probe then), and every
//!    [`PROBE_FAILED_RETRY_MS`] while the last one failed. A failed probe
//!    stops every workload and marks enabled installations `probe_failed`:
//!    nothing runs on a cluster that does not enforce the policy.
//!    An inconclusive probe (it could not tell) starts nothing and is run
//!    again next pass; a workload already running keeps running until its
//!    last passed probe is [`INCONCLUSIVE_HOLD_AFTER_MS`] old (the re-probe
//!    age plus [`INCONCLUSIVE_GRACE_MS`]), and is then held as a failed
//!    probe holds it.
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
use scarab_plugin_k8s::probe::ProbeVerdict;
use scarab_plugin_k8s::{K8sPluginRuntime, RuntimeError, WorkloadState};

/// The lease gating the loop to one replica at a time.
pub const PLUGIN_RUNTIME_LEASE: &str = "plugin-runtime";
/// Short, so a crashed leader is replaced within a minute (a clean shutdown
/// releases the lease at once). A pass can run far longer — a probe
/// alone is bounded at 5 minutes — so while a pass runs the lease is renewed
/// in the background every [`LEASE_RENEW_INTERVAL`], and re-checked before
/// anything is held or stopped on the probe's strength.
const PLUGIN_RUNTIME_LEASE_TTL_MS: i64 = 60 * 1000;
/// A third of the TTL: two renewals can fail before the lease lapses.
const LEASE_RENEW_INTERVAL: Duration =
    Duration::from_millis((PLUGIN_RUNTIME_LEASE_TTL_MS / 3) as u64);
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(15);
/// A passed probe is trusted this long; a CNI can change under a running
/// cluster.
pub const PROBE_MAX_AGE_MS: i64 = 6 * 60 * 60 * 1000;
/// How long probes may stay inconclusive after a passed probe falls due for
/// its re-probe ([`PROBE_MAX_AGE_MS`]) before running Plugins are held.
pub const INCONCLUSIVE_GRACE_MS: i64 = 6 * 60 * 60 * 1000;
/// How old the last passed probe may be while probes are inconclusive: the
/// re-probe age plus the grace, so a scheduled re-probe that cannot tell
/// does not hold anything at once. Past this the Plugin is held as a failed
/// probe holds it, until a probe passes. Measured from the installation's
/// `probe_passed_at`, which the store keeps, so a server restart does not
/// reset it.
pub const INCONCLUSIVE_HOLD_AFTER_MS: i64 = PROBE_MAX_AGE_MS + INCONCLUSIVE_GRACE_MS;
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
    /// `None` = passed (or inconclusive); `Some(why)` = failed.
    pub failure: Option<String>,
    /// `Some(why)` = the last probe could not tell (git-bug da0af59): what
    /// runs under a probe that passed less than
    /// [`INCONCLUSIVE_HOLD_AFTER_MS`] ago keeps running, what runs under an
    /// older one is held, nothing new starts, and the next pass probes
    /// again.
    pub inconclusive: Option<String>,
    /// What the last probe saw that does not decide the verdict (a reached
    /// kubelet), shown on a running Plugin's status (git-bug da0af59).
    pub notes: Vec<String>,
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
    if fresh || isolation.inconclusive.is_some() {
        return true;
    }
    isolation.failure.is_some() && age >= PROBE_FAILED_RETRY_MS
}

/// What this pass learned about the plugin namespace's baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Baseline {
    /// Fit to run Plugins in; the runtime's policies are applied, and a
    /// Plugin Pod reaches the broker at `pod_broker_url` (by IP: a Plugin
    /// has no DNS).
    Ready { pod_broker_url: String },
    /// The namespace is not one Plugins may run in (not `restricted`, a
    /// stray policy, no broker peer). Fail closed: hold every workload.
    Refused(String),
    /// The cluster could not be asked — a transient Kubernetes error. It
    /// proves nothing about the namespace, so it must not stop the fleet:
    /// this pass leaves everything but Killed installations as it is.
    Unavailable(String),
}

impl Baseline {
    /// Classify one pass's baseline check — followed, when it passed, by
    /// the broker's address as a Pod dials it. Only a refusal holds
    /// workloads; any other error — a Kubernetes error, a broker name that
    /// did not resolve this pass — is "could not ask".
    pub fn classify(checked: Result<String, RuntimeError>) -> Self {
        match checked {
            Ok(pod_broker_url) => Baseline::Ready { pod_broker_url },
            Err(e @ RuntimeError::BaselineRefused(_)) => Baseline::Refused(e.to_string()),
            Err(
                e @ (RuntimeError::Kube(_)
                | RuntimeError::Invalid(_)
                | RuntimeError::BrokerUnresolved(_)
                | RuntimeError::Storage(_)),
            ) => Baseline::Unavailable(e.to_string()),
        }
    }
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
    /// Apply its workload, dialing the broker at `pod_broker_url`;
    /// `probe_passed_at` is the probe that allowed it.
    Run {
        probe_passed_at: i64,
        pod_broker_url: String,
    },
    /// Not started, nor stopped: the last probe was inconclusive and this
    /// installation has no earlier passed probe to run under. Record
    /// `pending` with why; the next pass probes again.
    Defer { message: String },
    /// Leave it as it is, and write nothing: the cluster could not be asked
    /// this pass ([`Baseline::Unavailable`]), or there is no probe result
    /// yet. (The second is defensive: [`probe_due`] forces a probe whenever
    /// something is enabled and none ran since this replica took the lease,
    /// so the loop never reaches it.)
    Wait,
}

/// Decide one installation from the namespace baseline and the isolation
/// result at `now` (Unix-ms). Killed always stops — even when the cluster
/// is flaky, since a stop is the safe direction; nothing enabled runs
/// without both a baseline and a passed probe (one at most
/// [`INCONCLUSIVE_HOLD_AFTER_MS`] old while the probe is inconclusive); and
/// a baseline that could not be checked changes nothing.
pub fn decide(
    installation: &PluginInstallation,
    baseline: &Baseline,
    isolation: &Isolation,
    now: i64,
) -> Decision {
    if installation.state == InstallationState::Killed {
        return Decision::Stop;
    }
    let pod_broker_url = match baseline {
        Baseline::Ready { pod_broker_url } => pod_broker_url,
        Baseline::Refused(why) => {
            return Decision::Hold {
                status: RuntimeStatus::Error,
                message: why.clone(),
            }
        }
        Baseline::Unavailable(_) => return Decision::Wait,
    };
    if let (None, Some(why)) = (&isolation.failure, &isolation.inconclusive) {
        // Proves nothing: a workload running under an earlier passed probe
        // keeps running under it, for a while; nothing new starts; and a
        // hold stays as it is, with its finding, until a probe passes.
        if installation.held_by_the_runtime() {
            return Decision::Wait;
        }
        return match installation.probe_passed_at {
            // No probe has passed for too long: hold it as a failed probe
            // would. A passed probe releases it the same way.
            Some(earlier) if now.saturating_sub(earlier) >= INCONCLUSIVE_HOLD_AFTER_MS => {
                Decision::Hold {
                    status: RuntimeStatus::ProbeFailed,
                    message: format!(
                        "held: no isolation probe has passed for {}h ({why})",
                        INCONCLUSIVE_HOLD_AFTER_MS / (60 * 60 * 1000)
                    ),
                }
            }
            Some(earlier) => Decision::Run {
                probe_passed_at: earlier,
                pod_broker_url: pod_broker_url.clone(),
            },
            None => Decision::Defer {
                message: why.clone(),
            },
        };
    }
    match (&isolation.failure, isolation.probed_at) {
        (Some(why), _) => Decision::Hold {
            status: RuntimeStatus::ProbeFailed,
            message: why.clone(),
        },
        (None, Some(at)) => Decision::Run {
            probe_passed_at: at,
            pod_broker_url: pod_broker_url.clone(),
        },
        (None, None) => Decision::Wait,
    }
}

/// A running Plugin's status message carries the last probe's notes (a
/// reached kubelet), so an operator sees them; any other status keeps its
/// own message.
fn with_notes(
    (status, message): (RuntimeStatus, String),
    isolation: &Isolation,
) -> (RuntimeStatus, String) {
    if status == RuntimeStatus::Running && message.is_empty() && !isolation.notes.is_empty() {
        (status, format!("note: {}", isolation.notes.join("; ")))
    } else {
        (status, message)
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

/// Renews the lease in the background while a pass runs, so a pass longer
/// than the TTL is never taken over midway — a second replica sweeping this
/// one's probe Pods, or stopping what it just started. Dropping it stops the
/// renewing, on every exit from the pass.
struct LeaseRenewer(tokio::task::JoinHandle<()>);

impl LeaseRenewer {
    fn spawn(db: Arc<dyn Db>, owner: String) -> Self {
        Self(tokio::spawn(async move {
            loop {
                tokio::time::sleep(LEASE_RENEW_INTERVAL).await;
                match db
                    .lease(PLUGIN_RUNTIME_LEASE, &owner, PLUGIN_RUNTIME_LEASE_TTL_MS)
                    .await
                {
                    Ok(lease) if lease.owner == owner => {}
                    Ok(lease) => {
                        // The pass notices at its own ownership check, before
                        // it holds or stops anything.
                        tracing::warn!(
                            holder = %lease.owner,
                            "plugin runtime: lost the lease during a pass"
                        );
                        break;
                    }
                    // A database blip: keep trying while the lease lasts.
                    Err(e) => {
                        tracing::warn!(error = %e, "plugin runtime: could not renew the lease")
                    }
                }
            }
        }))
    }
}

impl Drop for LeaseRenewer {
    fn drop(&mut self) {
        self.0.abort();
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

    /// Run a pass every [`RECONCILE_INTERVAL`] until `stop` resolves, then
    /// release the lease so another replica takes over at once rather than
    /// after the TTL (git-bug da0af59). A pass under way when `stop`
    /// resolves is abandoned; the next leader starts its own.
    pub fn spawn_until(
        mut self,
        stop: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tokio::pin!(stop);
            loop {
                tokio::select! {
                    _ = &mut stop => break,
                    result = self.reconcile_once() => {
                        if let Err(e) = result {
                            tracing::warn!(error = %e, "plugin runtime: reconcile pass failed");
                        }
                    }
                }
                tokio::select! {
                    _ = &mut stop => break,
                    _ = tokio::time::sleep(RECONCILE_INTERVAL) => {}
                }
            }
            if let Err(e) = self
                .db
                .release_lease(PLUGIN_RUNTIME_LEASE, &self.owner)
                .await
            {
                tracing::warn!(error = %e, "plugin runtime: could not release the lease");
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
        let _renewer = LeaseRenewer::spawn(self.db.clone(), self.owner.clone());
        self.pass().await
    }

    /// Whether this replica still holds the lease (renewing it if so). A
    /// replica that lost it forgets its probe result, exactly as one that
    /// never held it.
    async fn still_holds_lease(&mut self) -> Result<bool, String> {
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
            tracing::warn!(holder = %lease.owner, "plugin runtime: another replica holds the lease; abandoning this pass");
            self.isolation = Isolation::default();
            return Ok(false);
        }
        Ok(true)
    }

    /// The pass proper, under the lease.
    async fn pass(&mut self) -> Result<(), String> {
        let installations = self
            .store
            .all_installations()
            .await
            .map_err(|e| e.to_string())?;

        let checked = match self.runtime.ensure_baseline().await {
            Ok(()) => self.runtime.pod_broker_url().await,
            Err(e) => Err(e),
        };
        let baseline = Baseline::classify(checked);
        match &baseline {
            Baseline::Refused(why) => {
                tracing::error!(reason = %why, "plugin runtime: namespace baseline refused; no plugin runs");
                // Nothing proven about the namespace any more.
                self.isolation = Isolation::default();
            }
            Baseline::Unavailable(why) => {
                // Not a verdict on the namespace: keep the isolation result,
                // leave running workloads up, and try again next pass.
                tracing::warn!(
                    error = %why,
                    "plugin runtime: could not check the namespace baseline; skipping this pass \
                     (killed installations are still stopped)"
                );
            }
            Baseline::Ready { pod_broker_url } => {
                let now = self.clock.now().await.0;
                if probe_due(now, &self.isolation, &installations) {
                    self.probe(&installations, pod_broker_url).await;
                }
            }
        }
        // The probe can take minutes. Before anything is held or stopped on
        // its strength, make sure no other replica took over meanwhile: two
        // runtimes acting on one namespace would undo each other.
        if !self.still_holds_lease().await? {
            return Ok(());
        }

        // After the probe: it can take minutes, and the hold below measures
        // from the last passed probe.
        let now = self.clock.now().await.0;
        for installation in &installations {
            if let Err(e) = self
                .reconcile_installation(installation, &baseline, now)
                .await
            {
                tracing::warn!(
                    installation = %installation.id,
                    org = %installation.org,
                    name = %installation.name,
                    error = %e,
                    "plugin runtime: reconcile failed for an installation"
                );
            }
        }

        // A flaky cluster this pass: no orphan sweep either — its listing
        // would come from the same API that just failed.
        if matches!(baseline, Baseline::Unavailable(_)) {
            return Ok(());
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

    async fn probe(&mut self, installations: &[PluginInstallation], pod_broker_url: &str) {
        // Shown while it runs, for what is waiting on it — installs and
        // enables, not workloads already running under an earlier probe.
        for i in installations.iter().filter(|i| {
            i.state == InstallationState::Enabled
                && i.runtime_status == RuntimeStatus::Pending
                && i.probe_passed_at.is_none()
        }) {
            let _ = self
                .store
                .set_runtime_status(
                    &i.id,
                    InstallationState::Enabled,
                    i.state_version,
                    RuntimeStatus::Probing,
                    "isolation probe running",
                    i.probe_passed_at,
                )
                .await;
        }
        let report = self.runtime.probe_isolation(pod_broker_url).await;
        let finished = self.clock.now().await.0;
        match report.verdict {
            ProbeVerdict::Passed => tracing::info!(
                result = %report.message(),
                "plugin runtime: isolation probe passed"
            ),
            ProbeVerdict::Failed => tracing::error!(
                findings = %report.message(),
                "plugin runtime: isolation probe FAILED; no plugin runs"
            ),
            ProbeVerdict::Inconclusive => tracing::warn!(
                findings = %report.message(),
                "plugin runtime: isolation probe inconclusive; running plugins keep running \
                 until 12h after their last passed probe, none starts, probing again next pass"
            ),
        }
        self.isolation = Isolation {
            probed_at: Some(finished),
            failure: (report.verdict == ProbeVerdict::Failed).then(|| report.message()),
            inconclusive: (report.verdict == ProbeVerdict::Inconclusive).then(|| report.message()),
            notes: report.notes.clone(),
        };
    }

    /// Stop the installation's workload, unless it is already down: a
    /// killed or held installation is visited every pass, and its stop is a
    /// ServiceAccount delete and a Deployment patch (git-bug da0af59).
    async fn stop_unless_stopped(&self, installation_id: &str) -> Result<(), String> {
        if self
            .runtime
            .is_stopped(installation_id)
            .await
            .map_err(|e| e.to_string())?
        {
            return Ok(());
        }
        self.runtime
            .stop(installation_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn reconcile_installation(
        &self,
        installation: &PluginInstallation,
        baseline: &Baseline,
        now: i64,
    ) -> Result<(), String> {
        let (status, message, probe_passed_at) =
            match decide(installation, baseline, &self.isolation, now) {
                Decision::Wait => return Ok(()),
                Decision::Stop => {
                    self.stop_unless_stopped(&installation.id).await?;
                    (
                        RuntimeStatus::Stopped,
                        String::new(),
                        installation.probe_passed_at,
                    )
                }
                Decision::Hold { status, message } => {
                    self.stop_unless_stopped(&installation.id).await?;
                    (status, message, None)
                }
                Decision::Defer { message } => (RuntimeStatus::Pending, message, None),
                Decision::Run {
                    probe_passed_at,
                    pod_broker_url,
                } => {
                    let applied = self.runtime.ensure(installation, &pod_broker_url).await;
                    let (status, message) = match applied {
                        // A failed read-back is not a failed workload: the
                        // apply landed. Record `pending` with why, and —
                        // crucially — the probe that allowed it, so the next
                        // pass does not take it for a fresh install and
                        // re-probe.
                        Ok(()) => match self.runtime.workload_state(installation).await {
                            Ok(state) => with_notes(running_status(&state), &self.isolation),
                            Err(e) => (RuntimeStatus::Pending, format!("state unknown: {e}")),
                        },
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
                    installation.state_version,
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
                image: None,
                capabilities: vec![PluginCapability::UseConnection],
                connections: vec![],
                config_keys: vec![],
                gates: Default::default(),
            },
            manifest_sha256: "0".repeat(64),
            source: None,
            source_connection: None,
            config: BTreeMap::new(),
            state,
            state_version: 0,
            runtime_status: RuntimeStatus::Pending,
            runtime_message: String::new(),
            probe_passed_at: None,
            approved_by: "amy".into(),
            approved_at: 1,
            killed_by: None,
            killed_at: None,
            storage: None,
        }
    }

    fn running(probed_at: i64) -> PluginInstallation {
        PluginInstallation {
            runtime_status: RuntimeStatus::Running,
            probe_passed_at: Some(probed_at),
            ..installation(InstallationState::Enabled)
        }
    }

    fn ready() -> Baseline {
        Baseline::Ready {
            pod_broker_url: "http://10.96.0.20:8090".into(),
        }
    }

    fn passed(at: i64) -> Isolation {
        Isolation {
            probed_at: Some(at),
            failure: None,
            inconclusive: None,
            notes: Vec::new(),
        }
    }

    fn failed(at: i64) -> Isolation {
        Isolation {
            probed_at: Some(at),
            failure: Some("isolation probe failed: NetworkPolicy is NOT enforced".into()),
            inconclusive: None,
            notes: Vec::new(),
        }
    }

    fn inconclusive(at: i64) -> Isolation {
        Isolation {
            probed_at: Some(at),
            failure: None,
            inconclusive: Some("isolation probe inconclusive: the probe could not run".into()),
            notes: Vec::new(),
        }
    }

    /// git-bug da0af59 (6): an inconclusive probe keeps a workload running
    /// under its earlier passed probe, starts nothing new, holds nothing,
    /// and is retried on the next pass; a failed one still holds.
    #[test]
    fn an_inconclusive_probe_keeps_running_plugins_and_starts_none() {
        let now = 100 * HOUR_MS;
        assert_eq!(
            decide(&running(now - HOUR_MS), &ready(), &inconclusive(now), now),
            Decision::Run {
                probe_passed_at: now - HOUR_MS,
                pod_broker_url: "http://10.96.0.20:8090".into(),
            }
        );
        let fresh = installation(InstallationState::Enabled);
        assert_eq!(
            decide(&fresh, &ready(), &inconclusive(now), now),
            Decision::Defer {
                message: "isolation probe inconclusive: the probe could not run".into()
            }
        );
        assert!(matches!(
            decide(&running(now - HOUR_MS), &ready(), &failed(now), now),
            Decision::Hold {
                status: RuntimeStatus::ProbeFailed,
                ..
            }
        ));
        assert_eq!(
            decide(
                &installation(InstallationState::Killed),
                &ready(),
                &inconclusive(now),
                now
            ),
            Decision::Stop
        );
        assert!(probe_due(
            now + 1,
            &inconclusive(now),
            &[running(now - HOUR_MS)]
        ));
    }

    /// git-bug da0af59 (9, red team): a probe's note reaches a running
    /// Plugin's message; other statuses keep theirs.
    #[test]
    fn a_probe_note_reaches_a_running_plugins_message() {
        let mut noted = passed(1);
        noted.notes = vec!["a plugin-labelled Pod reached the kubelet".into()];
        assert_eq!(
            with_notes((RuntimeStatus::Running, String::new()), &noted),
            (
                RuntimeStatus::Running,
                "note: a plugin-labelled Pod reached the kubelet".to_string()
            )
        );
        assert_eq!(
            with_notes((RuntimeStatus::Error, "CrashLoopBackOff".into()), &noted),
            (RuntimeStatus::Error, "CrashLoopBackOff".to_string())
        );
        assert_eq!(
            with_notes((RuntimeStatus::Running, String::new()), &passed(1)),
            (RuntimeStatus::Running, String::new())
        );
    }

    /// git-bug da0af59 (6, red team): an inconclusive retry leaves a hold
    /// as it is; only a passed probe lifts it.
    #[test]
    fn an_inconclusive_probe_does_not_lift_a_hold() {
        let now = 100 * HOUR_MS;
        let mut held = installation(InstallationState::Enabled);
        held.runtime_status = RuntimeStatus::ProbeFailed;
        held.probe_passed_at = None;
        assert_eq!(
            decide(&held, &ready(), &inconclusive(now), now),
            Decision::Wait
        );
        held.runtime_status = RuntimeStatus::Error;
        assert_eq!(
            decide(&held, &ready(), &inconclusive(now), now),
            Decision::Wait
        );
        assert!(matches!(
            decide(&held, &ready(), &passed(now), now),
            Decision::Run { .. }
        ));
    }

    /// An inconclusive probe keeps a Plugin running only while its last
    /// passed probe is under 12 h old (the 6 h re-probe age plus 6 h of
    /// grace); past that the Plugin is held as a failed probe holds it,
    /// further inconclusive probes leave the hold, and a passed probe
    /// releases it.
    #[test]
    fn inconclusive_probes_hold_a_plugin_twelve_hours_after_the_last_pass() {
        let passed_at = 100 * HOUR_MS;
        let plugin = running(passed_at);

        // The scheduled re-probe at 6 h cannot tell: it keeps running.
        let due = passed_at + PROBE_MAX_AGE_MS;
        assert!(matches!(
            decide(&plugin, &ready(), &inconclusive(due), due),
            Decision::Run { .. }
        ));
        // Inside the window: it keeps running.
        let within = passed_at + INCONCLUSIVE_HOLD_AFTER_MS - 1;
        assert_eq!(
            decide(&plugin, &ready(), &inconclusive(within), within),
            Decision::Run {
                probe_passed_at: passed_at,
                pod_broker_url: "http://10.96.0.20:8090".into(),
            }
        );

        // Twelve hours without a pass: held.
        let past = passed_at + INCONCLUSIVE_HOLD_AFTER_MS;
        let Decision::Hold { status, message } =
            decide(&plugin, &ready(), &inconclusive(past), past)
        else {
            panic!("expected a hold twelve hours after the last passed probe");
        };
        assert_eq!(status, RuntimeStatus::ProbeFailed);
        assert_eq!(
            message,
            "held: no isolation probe has passed for 12h \
             (isolation probe inconclusive: the probe could not run)"
        );

        // What the pass records for a hold: stopped, the probe cleared.
        let held = PluginInstallation {
            runtime_status: status,
            runtime_message: message,
            probe_passed_at: None,
            ..plugin
        };
        assert!(held.held_by_the_runtime());
        let later = past + HOUR_MS;
        assert_eq!(
            decide(&held, &ready(), &inconclusive(later), later),
            Decision::Wait
        );

        // A passed probe releases it, as it releases a failed-probe hold.
        assert_eq!(
            decide(&held, &ready(), &passed(later), later),
            Decision::Run {
                probe_passed_at: later,
                pod_broker_url: "http://10.96.0.20:8090".into(),
            }
        );
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
                    &ready(),
                    &isolation,
                    1
                ),
                Decision::Stop
            );
        }
        for baseline in [
            Baseline::Refused("refused".into()),
            Baseline::Unavailable("kubernetes: timeout".into()),
        ] {
            assert_eq!(
                decide(
                    &installation(InstallationState::Killed),
                    &baseline,
                    &passed(1),
                    1
                ),
                Decision::Stop
            );
        }
    }

    /// A Kubernetes error while checking the baseline proves nothing about
    /// the namespace: it must not stop running workloads (and revoke their
    /// tokens) the way a refusal does.
    #[test]
    fn a_transient_kubernetes_error_leaves_running_plugins_alone() {
        let baseline = Baseline::classify(Err(RuntimeError::Kube("503".into())));
        assert!(matches!(baseline, Baseline::Unavailable(_)));
        assert_eq!(
            decide(&running(1), &baseline, &passed(1), 1),
            Decision::Wait
        );
        // Only a refusal holds.
        let refused =
            Baseline::classify(Err(RuntimeError::BaselineRefused("not restricted".into())));
        assert!(matches!(
            decide(&running(1), &refused, &passed(1), 1),
            Decision::Hold {
                status: RuntimeStatus::Error,
                ..
            }
        ));
        // A broker name that did not resolve this pass is "could not ask"
        // too: plugins already running keep the address they were given.
        let unresolved = Baseline::classify(Err(RuntimeError::BrokerUnresolved(
            "`scarab-broker`: timeout".into(),
        )));
        assert!(matches!(unresolved, Baseline::Unavailable(_)));
        assert_eq!(
            decide(&running(1), &unresolved, &passed(1), 1),
            Decision::Wait
        );
    }

    #[test]
    fn nothing_runs_without_a_baseline_and_a_passed_probe() {
        let enabled = installation(InstallationState::Enabled);
        assert_eq!(
            decide(
                &enabled,
                &Baseline::Refused("not restricted".into()),
                &passed(1),
                1
            ),
            Decision::Hold {
                status: RuntimeStatus::Error,
                message: "not restricted".into()
            }
        );
        assert!(matches!(
            decide(&enabled, &ready(), &failed(1), 1),
            Decision::Hold {
                status: RuntimeStatus::ProbeFailed,
                ..
            }
        ));
        assert_eq!(
            decide(&enabled, &ready(), &Isolation::default(), 1),
            Decision::Wait
        );
        assert_eq!(
            decide(&enabled, &ready(), &passed(7), 1),
            Decision::Run {
                probe_passed_at: 7,
                pod_broker_url: "http://10.96.0.20:8090".into()
            }
        );
    }

    #[test]
    fn a_long_kubernetes_error_is_capped_for_the_ui() {
        let long = "x".repeat(5000);
        assert_eq!(capped(&long).chars().count(), MAX_RUNTIME_MESSAGE_CHARS);
        assert_eq!(capped("short"), "short");
    }
}
