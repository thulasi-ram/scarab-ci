//! [`K8sPluginRuntime`]: the plugin namespace's baseline, the isolation probe,
//! and one workload per PluginInstallation (ADR-0072 §9).

use std::collections::BTreeSet;
use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Namespace, Pod, ServiceAccount};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};

use scarab_plugin::{installation_id_of_service_account, PluginInstallation};

use crate::objects::{self, LABEL_PROBE};
use crate::probe::{self, ProbeReport};
use crate::{BrokerPeer, FIELD_MANAGER, LABEL_INSTALLATION, PSA_ENFORCE_LABEL, PSA_RESTRICTED};

/// How long the canary may take to accept connections (an image pull on a
/// cold node included), and how long the two checking Pods may take to
/// finish their scripts. Both inside [`probe::PROBE_POD_DEADLINE_SECONDS`].
const CANARY_READY_TIMEOUT: Duration = Duration::from_secs(120);
const PROBE_PODS_TIMEOUT: Duration = Duration::from_secs(180);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What the runtime needs to know, from `SCARAB_PLUGIN_*`.
#[derive(Debug, Clone)]
pub struct PluginRuntimeConfig {
    /// The one shared plugin namespace (`SCARAB_PLUGIN_NAMESPACE`).
    pub namespace: String,
    /// What a Plugin Pod dials (`SCARAB_PLUGIN_BROKER_URL`).
    pub broker_url: String,
    /// How `plugin-egress` names the broker (`SCARAB_PLUGIN_BROKER_PEER`).
    /// `None` = not configured: the baseline refuses, and nothing starts.
    pub broker_peer: Option<BrokerPeer>,
    /// The broker listener's port (`SCARAB_BROKER_ADDR`'s): the port a
    /// Plugin's packets arrive on at the broker, after any Service
    /// translation — what a NetworkPolicy port means.
    pub broker_port: u16,
    /// The isolation probe's image (`SCARAB_PLUGIN_PROBE_IMAGE`).
    pub probe_image: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    /// The namespace is not one Plugins may run in. Fail closed: nothing
    /// starts until an operator fixes it.
    #[error("the plugin namespace is refused: {0}")]
    BaselineRefused(String),
    /// The installation cannot be turned into a workload.
    #[error("{0}")]
    Invalid(String),
    #[error("kubernetes: {0}")]
    Kube(String),
}

fn kube_err(e: kube::Error) -> RuntimeError {
    RuntimeError::Kube(e.to_string())
}

/// A 404 is the outcome a delete or a scale-down wanted.
fn gone_is_fine<T>(r: Result<T, kube::Error>) -> Result<(), RuntimeError> {
    match r {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(status)) if status.code == 404 => Ok(()),
        Err(e) => Err(kube_err(e)),
    }
}

/// An installation's workload as the cluster has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkloadState {
    /// No Deployment.
    Absent,
    /// Scaled to zero.
    Stopped,
    /// Rolling out; not available yet. The text says what it is waiting on.
    Starting(String),
    Available,
    /// It cannot come up as it is: an image that cannot be pulled, a
    /// crash loop, a Pod the namespace's admission refused, a rollout past
    /// its deadline.
    Failing(String),
}

/// Container waiting reasons that will not clear on their own.
const STUCK_REASONS: [&str; 6] = [
    "ImagePullBackOff",
    "ErrImagePull",
    "InvalidImageName",
    "CrashLoopBackOff",
    "CreateContainerConfigError",
    "CreateContainerError",
];

/// Runs Plugins in one namespace. Every method is idempotent: the reconcile
/// loop calls them every pass and a repeat changes nothing.
pub struct K8sPluginRuntime {
    client: Client,
    config: PluginRuntimeConfig,
}

impl K8sPluginRuntime {
    pub fn new(client: Client, config: PluginRuntimeConfig) -> Self {
        Self { client, config }
    }

    pub fn config(&self) -> &PluginRuntimeConfig {
        &self.config
    }

    fn namespaced<K>(&self) -> Api<K>
    where
        K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>,
        <K as kube::Resource>::DynamicType: Default,
    {
        Api::namespaced(self.client.clone(), &self.config.namespace)
    }

    fn apply_params() -> PatchParams {
        PatchParams::apply(FIELD_MANAGER).force()
    }

    /// Make the namespace fit to run Plugins in, or refuse (ADR-0072 §9):
    ///
    /// - it must exist and **enforce** the `restricted` Pod Security level.
    ///   The runtime never labels it itself — it has no right to (the chart
    ///   grants `get` on that one namespace and nothing more), and a runtime
    ///   that could lower a namespace's level could be talked into it;
    /// - the broker peer must be configured;
    /// - it must hold no NetworkPolicy but the baseline's: policies are
    ///   additive, so a stray allow-all would silently undo the deny;
    /// - then the baseline policies are server-side-applied, deny first.
    pub async fn ensure_baseline(&self) -> Result<(), RuntimeError> {
        let ns = &self.config.namespace;
        let namespace = Api::<Namespace>::all(self.client.clone())
            .get_opt(ns)
            .await
            .map_err(kube_err)?
            .ok_or_else(|| {
                RuntimeError::BaselineRefused(format!(
                    "namespace `{ns}` does not exist; create it with the label \
                     {PSA_ENFORCE_LABEL}={PSA_RESTRICTED} (the chart's plugins.enabled does)"
                ))
            })?;
        let level = namespace
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(PSA_ENFORCE_LABEL));
        if level.map(String::as_str) != Some(PSA_RESTRICTED) {
            return Err(RuntimeError::BaselineRefused(format!(
                "namespace `{ns}` must enforce Pod Security level `{PSA_RESTRICTED}` \
                 ({PSA_ENFORCE_LABEL}={PSA_RESTRICTED}); it has {}",
                level.map_or("no enforce label".to_string(), |l| format!("`{l}`"))
            )));
        }
        let Some(peer) = &self.config.broker_peer else {
            return Err(RuntimeError::BaselineRefused(
                "SCARAB_PLUGIN_BROKER_PEER is not set, so no egress policy can name the broker"
                    .into(),
            ));
        };
        let wanted =
            objects::baseline_policies(&self.config.namespace, peer, self.config.broker_port);
        let policies: Api<NetworkPolicy> = self.namespaced();
        let wanted_names: BTreeSet<String> = wanted
            .iter()
            .filter_map(|np| np.metadata.name.clone())
            .collect();
        let strays: Vec<String> = policies
            .list(&ListParams::default())
            .await
            .map_err(kube_err)?
            .items
            .into_iter()
            .filter_map(|np| np.metadata.name)
            .filter(|name| !wanted_names.contains(name))
            .collect();
        if !strays.is_empty() {
            return Err(RuntimeError::BaselineRefused(format!(
                "namespace `{ns}` holds NetworkPolicies the runtime did not write ({}); \
                 policies are additive, so any of them could widen what a Plugin reaches",
                strays.join(", ")
            )));
        }
        for np in &wanted {
            let name = np.metadata.name.as_deref().unwrap_or_default();
            policies
                .patch(name, &Self::apply_params(), &Patch::Apply(np))
                .await
                .map_err(kube_err)?;
        }
        Ok(())
    }

    /// Run the isolation probe (see [`crate::probe`]). Never an `Err`: a
    /// probe that cannot finish is a FAILED probe, and the Plugin does not
    /// start. Its Pods are deleted afterwards, pass or fail, and so are any a
    /// crashed earlier probe left behind — but only ones past their deadline,
    /// never another manager's probe still in flight.
    pub async fn probe_isolation(&self) -> ProbeReport {
        let pods: Api<Pod> = self.namespaced();
        self.clear_stale_probe_pods(&pods).await;
        let nonce = uuid::Uuid::new_v4().simple().to_string()[..10].to_string();
        let report = match self.run_probe(&pods, &nonce).await {
            Ok(report) => report,
            Err(why) => ProbeReport::could_not_run(why),
        };
        let mine = ListParams::default().labels(&format!("{LABEL_PROBE}={nonce}"));
        if let Err(e) = pods
            .delete_collection(&DeleteParams::background().grace_period(0), &mine)
            .await
        {
            tracing::warn!(error = %e, %nonce, "isolation probe: could not delete its Pods");
        }
        report
    }

    /// Delete probe Pods older than [`probe::PROBE_POD_DEADLINE_SECONDS`]:
    /// by then their `activeDeadlineSeconds` has ended them, so they belong
    /// to no live probe. A younger one may be another replica's, mid-probe
    /// after a lease handover — deleting it would fail that probe, and a
    /// failed probe stops every Plugin.
    async fn clear_stale_probe_pods(&self, pods: &Api<Pod>) {
        let listed = match pods.list(&ListParams::default().labels(LABEL_PROBE)).await {
            Ok(listed) => listed.items,
            Err(e) => {
                tracing::warn!(error = %e, "isolation probe: could not list leftover probe Pods");
                return;
            }
        };
        let now = k8s_openapi::jiff::Timestamp::now().as_second();
        for name in stale_probe_pods(&listed, now) {
            if let Err(e) = gone_is_fine(
                pods.delete(&name, &DeleteParams::background().grace_period(0))
                    .await,
            ) {
                tracing::warn!(error = %e, pod = %name, "isolation probe: could not delete a leftover probe Pod");
            }
        }
    }

    async fn run_probe(&self, pods: &Api<Pod>, nonce: &str) -> Result<ProbeReport, String> {
        let ns = &self.config.namespace;
        let image = &self.config.probe_image;
        let create = |pod: Pod| async move {
            pods.create(&PostParams::default(), &pod)
                .await
                .map_err(|e| format!("creating a probe Pod: {e}"))
        };
        let canary = create(probe::canary_pod(ns, nonce, image)).await?;
        let canary_name = canary.metadata.name.unwrap_or_default();
        let canary_ip = wait_for_ready_ip(pods, &canary_name).await?;
        let control = create(probe::control_pod(ns, nonce, image, &canary_ip)).await?;
        let plugin = create(probe::plugin_probe_pod(
            ns,
            nonce,
            image,
            &canary_ip,
            &self.config.broker_url,
        ))
        .await?;
        let control_name = control.metadata.name.unwrap_or_default();
        let plugin_name = plugin.metadata.name.unwrap_or_default();
        let (control_log, plugin_log) = tokio::try_join!(
            finished_log(pods, &control_name),
            finished_log(pods, &plugin_name)
        )?;
        let report = probe::assess(&control_log, &plugin_log);
        if !report.passed() {
            tracing::warn!(
                control = %control_log.trim(),
                plugin = %plugin_log.trim(),
                "isolation probe failed"
            );
        }
        Ok(report)
    }

    /// Apply installation's ServiceAccount and Deployment (one replica).
    pub async fn ensure(&self, installation: &PluginInstallation) -> Result<(), RuntimeError> {
        installation
            .validate()
            .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        let name = objects::workload_name(&installation.id);
        let sa = objects::service_account(&self.config.namespace, installation);
        self.namespaced::<ServiceAccount>()
            .patch(&name, &Self::apply_params(), &Patch::Apply(&sa))
            .await
            .map_err(kube_err)?;
        let deployment = objects::deployment(
            &self.config.namespace,
            installation,
            &self.config.broker_url,
        );
        self.namespaced::<Deployment>()
            .patch(&name, &Self::apply_params(), &Patch::Apply(&deployment))
            .await
            .map_err(kube_err)?;
        Ok(())
    }

    /// The kill switch's runtime half: scale to zero and delete the
    /// ServiceAccount — which invalidates every token bound to it at the API
    /// server, so even a token still in a terminating Pod verifies as
    /// nothing. (The broker refuses a killed installation on its own,
    /// before any of this lands.) The Deployment stays, so an enable is a
    /// scale-up, not a re-creation.
    pub async fn stop(&self, installation_id: &str) -> Result<(), RuntimeError> {
        let name = objects::workload_name(installation_id);
        // A merge patch, not an apply: an apply of `{replicas: 0}` alone
        // would remove every other field this manager owns.
        let scale = serde_json::json!({ "spec": { "replicas": 0 } });
        gone_is_fine(
            self.namespaced::<Deployment>()
                .patch(
                    &name,
                    &PatchParams {
                        field_manager: Some(FIELD_MANAGER.to_string()),
                        ..Default::default()
                    },
                    &Patch::Merge(&scale),
                )
                .await,
        )?;
        gone_is_fine(
            self.namespaced::<ServiceAccount>()
                .delete(&name, &DeleteParams::default())
                .await,
        )
    }

    /// Uninstall: delete the Deployment (its Pods with it) and the
    /// ServiceAccount.
    pub async fn uninstall(&self, installation_id: &str) -> Result<(), RuntimeError> {
        let name = objects::workload_name(installation_id);
        gone_is_fine(
            self.namespaced::<Deployment>()
                .delete(&name, &DeleteParams::background())
                .await,
        )?;
        gone_is_fine(
            self.namespaced::<ServiceAccount>()
                .delete(&name, &DeleteParams::default())
                .await,
        )
    }

    /// Every installation id something of ours still exists for — a
    /// Deployment or a ServiceAccount carrying the installation label.
    pub async fn workload_ids(&self) -> Result<BTreeSet<String>, RuntimeError> {
        let lp = ListParams::default().labels(LABEL_INSTALLATION);
        let mut ids = BTreeSet::new();
        for d in self
            .namespaced::<Deployment>()
            .list(&lp)
            .await
            .map_err(kube_err)?
            .items
        {
            ids.extend(id_of(d.metadata.name.as_deref()));
        }
        for sa in self
            .namespaced::<ServiceAccount>()
            .list(&lp)
            .await
            .map_err(kube_err)?
            .items
        {
            ids.extend(id_of(sa.metadata.name.as_deref()));
        }
        Ok(ids)
    }

    /// What the cluster has of one installation's workload.
    pub async fn workload_state(
        &self,
        installation_id: &str,
    ) -> Result<WorkloadState, RuntimeError> {
        let name = objects::workload_name(installation_id);
        let Some(deployment) = self
            .namespaced::<Deployment>()
            .get_opt(&name)
            .await
            .map_err(kube_err)?
        else {
            return Ok(WorkloadState::Absent);
        };
        if deployment.spec.as_ref().and_then(|s| s.replicas) == Some(0) {
            return Ok(WorkloadState::Stopped);
        }
        let status = deployment.status.unwrap_or_default();
        if status.available_replicas.unwrap_or(0) >= 1 {
            return Ok(WorkloadState::Available);
        }
        for condition in status.conditions.unwrap_or_default() {
            let failed = (condition.type_ == "ReplicaFailure" && condition.status == "True")
                || (condition.type_ == "Progressing" && condition.status == "False");
            if failed {
                return Ok(WorkloadState::Failing(
                    condition
                        .message
                        .unwrap_or_else(|| condition.reason.unwrap_or(condition.type_)),
                ));
            }
        }
        let pods = self
            .namespaced::<Pod>()
            .list(&ListParams::default().labels(&format!("{LABEL_INSTALLATION}={installation_id}")))
            .await
            .map_err(kube_err)?;
        for pod in pods.items {
            for cs in pod
                .status
                .and_then(|s| s.container_statuses)
                .unwrap_or_default()
            {
                if let Some(waiting) = cs.state.and_then(|s| s.waiting) {
                    let reason = waiting.reason.unwrap_or_default();
                    if STUCK_REASONS.contains(&reason.as_str()) {
                        return Ok(WorkloadState::Failing(match waiting.message {
                            Some(m) if !m.is_empty() => format!("{reason}: {m}"),
                            _ => reason,
                        }));
                    }
                }
            }
        }
        Ok(WorkloadState::Starting(format!(
            "{}/1 replicas available",
            status.available_replicas.unwrap_or(0)
        )))
    }
}

/// The probe Pods created more than [`probe::PROBE_POD_DEADLINE_SECONDS`]
/// before `now` (Unix seconds). One with no creation time is left alone: it
/// cannot be shown to be dead.
fn stale_probe_pods(pods: &[Pod], now: i64) -> Vec<String> {
    pods.iter()
        .filter(|pod| {
            pod.metadata
                .creation_timestamp
                .as_ref()
                .is_some_and(|created| {
                    now - created.0.as_second() > probe::PROBE_POD_DEADLINE_SECONDS
                })
        })
        .filter_map(|pod| pod.metadata.name.clone())
        .collect()
}

fn id_of(name: Option<&str>) -> Option<String> {
    name.and_then(installation_id_of_service_account)
        .map(str::to_string)
}

/// Wait until `name` is Ready (the canary's readiness probe accepted a TCP
/// connection) and return its Pod IP.
async fn wait_for_ready_ip(pods: &Api<Pod>, name: &str) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + CANARY_READY_TIMEOUT;
    loop {
        let pod = pods
            .get(name)
            .await
            .map_err(|e| format!("reading the canary: {e}"))?;
        let status = pod.status.unwrap_or_default();
        if matches!(status.phase.as_deref(), Some("Failed") | Some("Succeeded")) {
            return Err(format!(
                "the canary exited ({})",
                status.message.unwrap_or_default()
            ));
        }
        let ready = status
            .conditions
            .unwrap_or_default()
            .iter()
            .any(|c| c.type_ == "Ready" && c.status == "True");
        if let (true, Some(ip)) = (ready, status.pod_ip.filter(|ip| !ip.is_empty())) {
            return Ok(ip);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "the canary was not Ready within {}s",
                CANARY_READY_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Wait for a checking Pod to finish and return its log.
async fn finished_log(pods: &Api<Pod>, name: &str) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + PROBE_PODS_TIMEOUT;
    loop {
        let pod = pods
            .get(name)
            .await
            .map_err(|e| format!("reading probe Pod {name}: {e}"))?;
        let phase = pod.status.and_then(|s| s.phase).unwrap_or_default();
        if phase == "Succeeded" || phase == "Failed" {
            return pods
                .logs(name, &LogParams::default())
                .await
                .map_err(|e| format!("reading probe Pod {name}'s log: {e}"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "probe Pod {name} did not finish within {}s (phase {phase:?})",
                PROBE_PODS_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

    fn probe_pod_created_at(name: &str, created: Option<i64>) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                creation_timestamp: created
                    .map(|secs| Time(k8s_openapi::jiff::Timestamp::from_second(secs).unwrap())),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// The leftover sweep must not kill another replica's probe in flight
    /// (a lease handover mid-probe): only Pods past the probe deadline go.
    #[test]
    fn only_probe_pods_past_their_deadline_are_swept() {
        let now = 1_000_000;
        let pods = [
            probe_pod_created_at("in-flight", Some(now - 30)),
            probe_pod_created_at(
                "at-the-deadline",
                Some(now - probe::PROBE_POD_DEADLINE_SECONDS),
            ),
            probe_pod_created_at(
                "abandoned",
                Some(now - probe::PROBE_POD_DEADLINE_SECONDS - 1),
            ),
            probe_pod_created_at("no-creation-time", None),
        ];
        assert_eq!(stale_probe_pods(&pods, now), vec!["abandoned".to_string()]);
    }
}
