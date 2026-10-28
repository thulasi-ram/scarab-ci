//! [`K8sPluginRuntime`]: the plugin namespace's baseline, the isolation probe,
//! and one workload per PluginInstallation (ADR-0072 §9).

use std::collections::BTreeSet;
use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{
    ContainerStatus, Namespace, PersistentVolumeClaim, Pod, ServiceAccount,
};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};

use scarab_plugin::{installation_id_of_service_account, PluginInstallation};

use crate::objects::{self, LABEL_PROBE};
use crate::probe::{self, ProbeReport};
use crate::{
    BrokerPeer, BrokerUrl, FIELD_MANAGER, LABEL_INSTALLATION, PSA_ENFORCE_LABEL, PSA_RESTRICTED,
};

/// How long the canary may take to accept connections (an image pull on a
/// cold node included), and how long the two checking Pods may take to
/// finish their scripts. Both inside [`probe::PROBE_POD_DEADLINE_SECONDS`].
const CANARY_READY_TIMEOUT: Duration = Duration::from_secs(120);
const PROBE_PODS_TIMEOUT: Duration = Duration::from_secs(180);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

// The plugin-labelled probe Pod repeats its checks up to its enforcement
// deadline, then runs one last pass (~35 s at most: a 5 s healthz, a 15 s DNS
// timeout, four 3 s connects), after its own start; waiting less than that
// would turn every slow-to-enforce cluster into "the probe could not run".
const _: () =
    assert!((probe::PROBE_ENFORCEMENT_DEADLINE_SECONDS as u64) + 60 < PROBE_PODS_TIMEOUT.as_secs());

/// What the runtime needs to know, from `SCARAB_PLUGIN_*`.
#[derive(Debug, Clone)]
pub struct PluginRuntimeConfig {
    /// The one shared plugin namespace (`SCARAB_PLUGIN_NAMESPACE`).
    pub namespace: String,
    /// The broker URL (`SCARAB_PLUGIN_BROKER_URL`). A Plugin Pod dials it
    /// by IP: see [`K8sPluginRuntime::pod_broker_url`].
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
    /// The broker URL's host did not resolve (this pass).
    #[error("resolving the broker: {0}")]
    BrokerUnresolved(String),
    /// The installation's storage claim cannot be mounted: it is missing, or
    /// not annotated for this installation (ADR-0074 §7). The workload is
    /// held at zero replicas until an operator fixes the claim.
    #[error("{0}")]
    Storage(String),
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

    /// The broker URL as a Plugin Pod dials it: by IP, because a Plugin has
    /// no DNS (see `plugin-egress`). A URL that names an IP is used as is
    /// (proc mode: `up.sh` resolves the host as a Pod sees it). A name — the
    /// chart's broker Service — is resolved HERE, by the control plane, on
    /// every call, so a re-created Service's new ClusterIP is picked up
    /// within a pass; the NetworkPolicy peer is unaffected, since it is
    /// evaluated on the Pod behind the Service, after translation.
    pub async fn pod_broker_url(&self) -> Result<String, RuntimeError> {
        let url = BrokerUrl::parse(&self.config.broker_url).map_err(RuntimeError::Invalid)?;
        if let Some(ip) = url.ip() {
            return Ok(url.with_ip(ip));
        }
        let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((url.host(), url.port()))
            .await
            .map_err(|e| RuntimeError::BrokerUnresolved(format!("`{}`: {e}", url.host())))?
            .collect();
        // IPv4 first: a single-stack cluster's Service is IPv4, and a
        // dual-stack one serves both.
        let ip = addrs
            .iter()
            .find(|a| a.is_ipv4())
            .or_else(|| addrs.first())
            .map(|a| a.ip())
            .ok_or_else(|| {
                RuntimeError::BrokerUnresolved(format!("`{}` has no address", url.host()))
            })?;
        Ok(url.with_ip(ip))
    }

    /// Run the isolation probe (see [`crate::probe`]). Never an `Err`: a
    /// probe that cannot finish is a FAILED probe, and the Plugin does not
    /// start. Its Pods are deleted afterwards, pass or fail, and so are any a
    /// crashed earlier probe left behind — but only ones past their deadline,
    /// never another manager's probe still in flight.
    ///
    /// `pod_broker_url` is [`Self::pod_broker_url`]'s answer this pass.
    pub async fn probe_isolation(&self, pod_broker_url: &str) -> ProbeReport {
        let pods: Api<Pod> = self.namespaced();
        self.clear_stale_probe_pods(&pods).await;
        let nonce = uuid::Uuid::new_v4().simple().to_string()[..10].to_string();
        let report = match self.run_probe(&pods, &nonce, pod_broker_url).await {
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

    async fn run_probe(
        &self,
        pods: &Api<Pod>,
        nonce: &str,
        pod_broker_url: &str,
    ) -> Result<ProbeReport, String> {
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
            pod_broker_url,
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
    /// `pod_broker_url` is [`Self::pod_broker_url`]'s answer this pass.
    ///
    /// With storage (ADR-0074 §7), the claim is checked first, every pass:
    /// one that is missing or not annotated for this installation is never
    /// mounted — the workload is scaled to zero and the error says why — so
    /// a claim created or re-annotated later is checked before any Pod
    /// mounts it.
    pub async fn ensure(
        &self,
        installation: &PluginInstallation,
        pod_broker_url: &str,
    ) -> Result<(), RuntimeError> {
        installation
            .validate()
            .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        let name = objects::workload_name(&installation.id);
        if let Some(storage) = &installation.storage {
            let claim = self
                .namespaced::<PersistentVolumeClaim>()
                .get_opt(&storage.claim)
                .await
                .map_err(kube_err)?;
            if let Some(why) = objects::storage_claim_refusal(
                &self.config.namespace,
                installation,
                storage,
                claim.as_ref(),
            ) {
                self.scale_to_zero(&name).await?;
                return Err(RuntimeError::Storage(why));
            }
        }
        let sa = objects::service_account(&self.config.namespace, installation);
        self.namespaced::<ServiceAccount>()
            .patch(&name, &Self::apply_params(), &Patch::Apply(&sa))
            .await
            .map_err(kube_err)?;
        let deployment = objects::deployment(
            &self.config.namespace,
            installation,
            pod_broker_url,
            &self.config.probe_image,
        );
        self.namespaced::<Deployment>()
            .patch(&name, &Self::apply_params(), &Patch::Apply(&deployment))
            .await
            .map_err(kube_err)?;
        Ok(())
    }

    /// The kill switch's runtime half: delete the ServiceAccount — which
    /// invalidates every token bound to it at the API server, so even a
    /// token still in a terminating Pod verifies as nothing — and scale to
    /// zero. (The broker refuses a killed installation on its own, before
    /// any of this lands.) The Deployment stays, so an enable is a
    /// scale-up, not a re-creation.
    ///
    /// Revocation first, and both always attempted: the token is the
    /// security-relevant half, so a failed scale-down (or the reverse) must
    /// never leave it valid. Errors are combined, and the next pass retries.
    pub async fn stop(&self, installation_id: &str) -> Result<(), RuntimeError> {
        let name = objects::workload_name(installation_id);
        let revoked = gone_is_fine(
            self.namespaced::<ServiceAccount>()
                .delete(&name, &DeleteParams::default())
                .await,
        );
        let scaled_down = self.scale_to_zero(&name).await;
        // `gone_is_fine` already says "kubernetes: …"; keep it said once.
        let detail = |e: RuntimeError| match e {
            RuntimeError::Kube(m) => m,
            other => other.to_string(),
        };
        match (revoked.map_err(detail), scaled_down.map_err(detail)) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), Ok(())) => Err(RuntimeError::Kube(format!(
                "deleting the ServiceAccount (its tokens stay valid until it is): {e}"
            ))),
            (Ok(()), Err(e)) => Err(RuntimeError::Kube(format!(
                "scaling to zero (the ServiceAccount is deleted): {e}"
            ))),
            (Err(revoke), Err(scale)) => Err(RuntimeError::Kube(format!(
                "deleting the ServiceAccount: {revoke}; scaling to zero: {scale}"
            ))),
        }
    }

    /// Scale the Deployment `name` to zero; a missing one is fine.
    async fn scale_to_zero(&self, name: &str) -> Result<(), RuntimeError> {
        // A merge patch, not an apply: an apply of `{replicas: 0}` alone
        // would remove every other field this manager owns.
        let scale = serde_json::json!({ "spec": { "replicas": 0 } });
        gone_is_fine(
            self.namespaced::<Deployment>()
                .patch(
                    name,
                    &PatchParams {
                        field_manager: Some(FIELD_MANAGER.to_string()),
                        ..Default::default()
                    },
                    &Patch::Merge(&scale),
                )
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
        installation: &PluginInstallation,
    ) -> Result<WorkloadState, RuntimeError> {
        let installation_id = installation.id.as_str();
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
            let status = pod.status.unwrap_or_default();
            // The isolation self-check first: while it fails, the Plugin's
            // own container never starts, so only here is the reason.
            if let Some(why) = status
                .init_container_statuses
                .iter()
                .flatten()
                .find_map(self_check_failure)
            {
                return Ok(WorkloadState::Failing(why));
            }
            for cs in status.container_statuses.unwrap_or_default() {
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
        // Not up and nothing stuck: an unbound storage claim keeps the Pod
        // Pending (unschedulable) — say so, naming the claim.
        if let Some(storage) = &installation.storage {
            match self
                .namespaced::<PersistentVolumeClaim>()
                .get_opt(&storage.claim)
                .await
                .map_err(kube_err)?
            {
                None => {
                    return Ok(WorkloadState::Failing(format!(
                        "storage: PersistentVolumeClaim `{}` does not exist in namespace `{}`; \
                         Scarab does not create it",
                        storage.claim, self.config.namespace
                    )))
                }
                Some(claim) => {
                    if let Some(waiting) = objects::storage_claim_waiting(storage, &claim) {
                        return Ok(WorkloadState::Starting(waiting));
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

/// Why the Pod's isolation self-check has failed, if it has: from its FIRST
/// failed run, not only once the kubelet backs off. The kubelet restarts a
/// failed init container at once the first time, so for the whole of that
/// second run (another full deadline) the state is `running` and only
/// `lastState` says it failed — waiting for `CrashLoopBackOff` reported a
/// Pod whose network was open for two minutes as merely "starting".
///
/// A run that has since passed (`terminated`, exit 0) clears it: the
/// Plugin then starts, and the replica count speaks for it. The verdict is
/// the check's last pass, which the kubelet keeps as the termination
/// message (`FallbackToLogsOnError`).
fn self_check_failure(init: &ContainerStatus) -> Option<String> {
    if init.name != probe::SELF_CHECK_CONTAINER {
        return None;
    }
    let state = init.state.as_ref();
    let current = state.and_then(|s| s.terminated.as_ref());
    if current.is_some_and(|t| t.exit_code == 0) {
        return None;
    }
    let failed_run = current
        .or_else(|| init.last_state.as_ref().and_then(|s| s.terminated.as_ref()))
        .filter(|t| t.exit_code != 0);
    let crash_looping = state
        .and_then(|s| s.waiting.as_ref())
        .and_then(|w| w.reason.as_deref())
        == Some("CrashLoopBackOff");
    if failed_run.is_none() && !crash_looping {
        return None;
    }
    let verdict = failed_run
        .and_then(|t| t.message.as_deref())
        .map(|m| m.trim().replace('\n', "; "))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "no verdict recorded".to_string());
    Some(format!(
        "the isolation self-check failed — this Pod's network on its node is not isolated \
         as plugin-egress requires, so the Plugin does not start: {verdict}"
    ))
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
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

    fn terminated(exit_code: i32, message: &str) -> ContainerState {
        ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code,
                message: Some(message.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn self_check(state: ContainerState, last: Option<ContainerState>) -> ContainerStatus {
        ContainerStatus {
            name: probe::SELF_CHECK_CONTAINER.to_string(),
            state: Some(state),
            last_state: last,
            ..Default::default()
        }
    }

    const OPEN: &str = "SCARAB_PROBE broker reached\nSCARAB_PROBE internet reached\n\
                        isolation self-check FAILED\n";

    /// The kind tier's bug: the first failed run is followed AT ONCE by a
    /// second one (no backoff yet), so `CrashLoopBackOff` came only after
    /// two deadlines and the workload read as "starting" all along. Every
    /// state after a failed run is failing, with the verdict.
    #[test]
    fn a_self_check_is_failing_from_its_first_failed_run() {
        let running = ContainerState {
            running: Some(ContainerStateRunning::default()),
            ..Default::default()
        };
        let backing_off = ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some("CrashLoopBackOff".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        for status in [
            self_check(terminated(1, OPEN), None),
            self_check(running, Some(terminated(1, OPEN))),
            self_check(backing_off, Some(terminated(1, OPEN))),
        ] {
            let why = self_check_failure(&status).expect("failing");
            assert!(why.contains("isolation self-check failed"), "{why}");
            assert!(why.contains("SCARAB_PROBE internet reached"), "{why}");
        }
    }

    #[test]
    fn a_self_check_still_running_or_passed_is_not_failing() {
        let running = ContainerState {
            running: Some(ContainerStateRunning::default()),
            ..Default::default()
        };
        assert_eq!(self_check_failure(&self_check(running, None)), None);
        // Passed on a retry: the earlier failure no longer holds the Plugin.
        assert_eq!(
            self_check_failure(&self_check(terminated(0, ""), Some(terminated(1, OPEN)))),
            None
        );
        // Another init container's failure is not the self-check's verdict.
        let mut other = self_check(terminated(1, OPEN), None);
        other.name = "something-else".to_string();
        assert_eq!(self_check_failure(&other), None);
    }

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
