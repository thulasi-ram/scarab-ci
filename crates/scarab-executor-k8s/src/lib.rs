//! Kubernetes adapter for the [`scarab_engine::Executor`] port (ADR-0004).
//!
//! Each Step runs as **one bare Pod** with `restartPolicy: Never` — a clean,
//! individually-addressable, re-creatable object. The orchestrator owns retries
//! (ADR-0020); this adapter only creates the Pod, reflects its status, and
//! deletes it on cancel. `launch` is **idempotent on the step's fence**: the Pod
//! name is derived deterministically from `{run, step, attempt}`, so a relaunch
//! after a control-plane crash re-attaches to the existing Pod rather than
//! starting a second one (the double-effect guard, ADR-0021).

/// The workspace token codec (ADR-0061): minted here for a Step Pod, verified
/// by the workspace service. Mint and verify live in ONE module on purpose —
/// the results token's message format is duplicated across two crates and that
/// is a standing drift hazard, not a pattern to copy.
pub mod workspace_token;

use async_trait::async_trait;
use futures::io::AsyncBufRead;
use futures::AsyncReadExt;
use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EmptyDirVolumeSource, EnvVar, ExecAction,
    HTTPGetAction, Pod, PodSpec, Probe, ResourceRequirements, SeccompProfile, SecurityContext,
    Service, ServicePort, ServiceSpec, TCPSocketAction, Volume, VolumeMount,
};
use k8s_openapi::api::networking::v1::{
    NetworkPolicy, NetworkPolicyIngressRule, NetworkPolicyPeer, NetworkPolicyPort,
    NetworkPolicySpec,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, AttachParams, DeleteParams, LogParams, Patch, PatchParams, PostParams};
use std::sync::Arc;

use scarab_engine::ports::{ExecHandle, ExecState, FailureClass, LogChunks};
use scarab_engine::{ExecError, Executor, RunId, StepRun, StepSpec};
use scarab_storage::{Cas, StorageError};

/// The step container's name in every step Pod (see [`build_pod`]). The log tail
/// pins the source to this container so a results-egress sidecar (ADR-0042) never
/// pollutes the step's log stream.
const STEP_CONTAINER: &str = "step";

/// The container name in a standalone shared-service Pod (see [`build_service_pod`]).
/// The service log tail pins its source to this container (ADR-0058 evidence).
const SERVICE_POD_CONTAINER: &str = "service";

/// How many bytes the log tail reads per chunk before handing it to the pipeline.
/// A modest buffer keeps live-tail latency low without a syscall per line.
const LOG_CHUNK_BYTES: usize = 8 * 1024;

/// Default graceful-cancel window: SIGTERM, then SIGKILL after this many seconds.
const CANCEL_GRACE_SECS: i64 = 30;

/// What a Step Pod needs to fetch its own input snapshots (ADR-0061 s3-feed).
///
/// Present ⇒ a Step with `needs:` gets a **Scarab-owned** init container
/// ([`DEFAULT_WSFETCH_IMAGE`]) that dials the workspace service directly.
/// Absent ⇒ such a Step **cannot be launched at all**
/// ([`workspace_feed_is_satisfiable`]), and that is deliberate: the
/// control-plane `kubectl exec` tar tunnel this replaced is deleted, not kept as
/// a fallback. ADR-0061 D2.3 draws that line explicitly — an eager path is
/// permitted as a *temporally ordered replacement*, never as a runtime branch,
/// because a fallback that works is a fallback that becomes permanent.
///
/// ⚠ **ADR-0061 s3-feed: DELETE ME with the node driver (git-bug 0628369).**
/// This whole type is the stepping stone. The driver mounts a snapshot lazily as
/// a read-only lower layer, at which point there is no fetcher, no image, and no
/// eager copy of anything.
#[derive(Debug, Clone)]
pub struct WorkspaceFetch {
    /// Base URL of the workspace service **as a Pod sees it**. In proc mode
    /// that is generally *not* the URL the host uses (see
    /// `deploy/local-proc/up.sh`).
    pub url: String,
    /// HMAC secret the workspace token is minted with — the same secret the
    /// service verifies with. Never the results-egress secret: see
    /// [`workspace_token`] for the three reasons that reuse is refused.
    pub token_secret: Vec<u8>,
    /// The fetcher image. Digest-pin in production, exactly like the clone image.
    pub fetcher_image: String,
}

/// Configuration for the trusted per-Pod results-egress sidecar (ADR-0042). When
/// present, each step Pod gains a shared `/scarab/results` volume and a sidecar
/// that reads the step's `<name>.json` files and does a confirmed, fence-token-
/// authenticated write to the control plane's results API. Absent = no capture
/// (the executor's `results` stays empty).
#[derive(Debug, Clone)]
pub struct ResultsEgress {
    /// Base URL of the control-plane results API (e.g. `http://scarab-server`).
    pub base_url: String,
    /// HMAC secret shared with the server; mints the per-step fence token.
    pub token_secret: Vec<u8>,
    /// The sidecar image (a lightweight `scarab` CLI image; the CLI performs the
    /// drain + confirmed POST). Kept configurable so the image is a deploy concern.
    pub sidecar_image: String,
}

/// Operator-owned placement config (ADR-0055): the cluster **baseline** stamped
/// onto every step Pod plus the named **PlacementProfile** registry a step selects
/// from. Owned by the operator (server/executor config), never by a pipeline.
/// Default (empty) = no placement mutation, preserving the pre-0055 behavior.
/// Deserializable so an operator supplies it as one gitops-managed file
/// (`SCARAB_PLACEMENT_CONFIG_FILE`), never in a pipeline.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PlacementConfig {
    /// A raw pod-spec fragment merged onto **every** step Pod first — the
    /// pain-killer for tainted clusters (default tolerations/nodeSelector). `None`
    /// = no baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<serde_json::Value>,
    /// Default container resources applied when a step requests none. A step's own
    /// `resources` wins per-field.
    #[serde(default)]
    pub default_resources: scarab_pipeline::Resources,
    /// The named profile registry. A step's `placement_profiles` are resolved
    /// against this by name; an unknown name fails the launch (fail-closed).
    #[serde(default)]
    pub profiles: Vec<scarab_pipeline::PlacementProfile>,
}

impl PlacementConfig {
    fn profile(&self, name: &str) -> Option<&scarab_pipeline::PlacementProfile> {
        self.profiles.iter().find(|p| p.name == name)
    }
}

/// A Kubernetes-backed executor. Holds an optional client so the composition
/// root can construct it without contacting an API server.
pub struct K8sExecutor {
    client: Option<kube::Client>,
    namespace: String,
    results_egress: Option<ResultsEgress>,
    /// Global default step deadline in seconds (ADR-0047), applied when a step
    /// declares no `timeout:`. Default 1h.
    default_step_timeout_secs: u32,
    /// The canonical scarab-clone image a clone step runs (ADR-0045) —
    /// digest-pinned in production, never the author's image.
    clone_image: String,
    /// The workspace CAS (ADR-0029/0045). When wired, every step Pod gets the
    /// `/workspace` machinery: an init container that **fetches** the merged
    /// `needs` snapshots straight from the workspace service (ADR-0061 s3-feed),
    /// and an egress sidecar the control plane snapshots back into the CAS after
    /// the step exits. `None` = no workspace flow (tests / object-store-less dev).
    workspace_cas: Option<Arc<dyn Cas>>,
    /// The workspace service a Step Pod fetches its inputs from (ADR-0061
    /// s3-feed). Required for any Step with `needs:` once the workspace flow is
    /// on — there is no control-plane feed path any more.
    workspace_fetch: Option<WorkspaceFetch>,
    /// The Depot drain handle (ADR-0064 control-plane half): the same workspace
    /// service `workspace_cas` uses as its warm tier, held concretely because
    /// the drain needs `flush` — a client capability, not a `Cas` port method.
    /// When wired, `drive_workspace` ingests WARM-first through this client and
    /// awaits `flush(published_root)` before annotating the Pod and releasing
    /// the sidecar. `None` = no Depot (object-store-only dev): the drain
    /// ingests straight into `workspace_cas`, which IS the cold store then —
    /// durability is direct and there is nothing to flush.
    workspace_depot: Option<Arc<scarab_workspace_client::WorkspaceClient>>,
    /// The artifact blob store (ADR-0052). When wired (and the workspace
    /// flow is on), every step Pod gets a `/scarab/artifacts` emptyDir that
    /// is harvested post-step: matching files upload as object blobs and the
    /// metadata rides a Pod annotation the orchestrator persists.
    artifact_store: Option<Arc<dyn scarab_storage::ObjectStore>>,
    /// Operator placement config (ADR-0055): baseline + PlacementProfile registry.
    placement: PlacementConfig,
    /// In-process FALLBACK anchors for the drain escalation clock (ticket
    /// 66c93be): Pod name — a pure function of the `{run, step, attempt}`
    /// fence, see [`pod_name`] — → first observed drain-failure epoch ms.
    /// Consulted ONLY when the durable
    /// [`ANNOTATION_WS_DRAIN_FIRST_FAILURE`] anchor can neither be read nor
    /// written (revoked patch RBAC, broken admission webhook): without it the
    /// annotation write fails on EVERY poll, `drain_failure_verdict` sees no
    /// anchor, and the verdict stays `Transient` forever — the Depot outage
    /// presents as a step-budget timeout, the exact disguise 4cf03d7 forbids.
    ///
    /// Accepted tradeoff: this map is in-memory, so a control-plane restart
    /// resets the fallback clock. That can only DELAY escalation, never
    /// fabricate it — strictly better than never escalating. Entries are
    /// dropped when a drain succeeds or the Attempt escalates, and
    /// [`select_drain_anchor`] prunes strays older than 2× the escalation
    /// window on every invocation, so the map cannot grow unbounded.
    ///
    /// The residual hole, stated honestly (ticket 66c93be): a restart
    /// MID-outage restarts this clock, so escalation via the fallback can
    /// take up to ~2× the window instead of 1× — a bounded delay. But a
    /// CRASHLOOPING control plane (each life shorter than the window) with
    /// permanently broken patch RBAC re-seeds the clock every life and NEVER
    /// escalates: for that intersection the disguise 4cf03d7 forbids is still
    /// possible. The fallback narrows the hole to exactly that intersection;
    /// it does not close it. Closing it needs a durable anchor that survives
    /// both failures at once, which is what the annotation already is —
    /// everywhere except under the RBAC outage itself.
    ws_drain_fallback_anchors: std::sync::Mutex<std::collections::HashMap<String, i64>>,
}

impl K8sExecutor {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            client: None,
            namespace: namespace.into(),
            results_egress: None,
            default_step_timeout_secs: DEFAULT_STEP_TIMEOUT_SECS,
            workspace_cas: None,
            workspace_fetch: None,
            workspace_depot: None,
            artifact_store: None,
            clone_image: DEFAULT_CLONE_IMAGE.to_string(),
            placement: PlacementConfig::default(),
            ws_drain_fallback_anchors: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn with_client(namespace: impl Into<String>, client: kube::Client) -> Self {
        Self {
            client: Some(client),
            namespace: namespace.into(),
            results_egress: None,
            default_step_timeout_secs: DEFAULT_STEP_TIMEOUT_SECS,
            workspace_cas: None,
            workspace_fetch: None,
            workspace_depot: None,
            artifact_store: None,
            clone_image: DEFAULT_CLONE_IMAGE.to_string(),
            placement: PlacementConfig::default(),
            ws_drain_fallback_anchors: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Set the operator placement config (ADR-0055): the baseline stamped on every
    /// step Pod and the named PlacementProfile registry steps select from.
    pub fn with_placement(mut self, placement: PlacementConfig) -> Self {
        self.placement = placement;
        self
    }

    /// Override the canonical scarab-clone image (ADR-0045).
    pub fn with_clone_image(mut self, image: impl Into<String>) -> Self {
        self.clone_image = image.into();
        self
    }

    /// Enable artifact collection (ADR-0052): harvest `/scarab/artifacts`
    /// post-step into `store` (requires the workspace flow — the harvest runs
    /// in its egress leg).
    pub fn with_artifact_store(mut self, store: Arc<dyn scarab_storage::ObjectStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    /// Enable the workspace CAS flow (ADR-0029/0045): give each step Pod a
    /// `/workspace` its `needs` snapshots are fetched into, and snapshot it back
    /// after the step exits.
    ///
    /// This wires the **drain** half. A Step with `needs:` additionally requires
    /// [`with_workspace_service`](Self::with_workspace_service) — the feed half
    /// now happens inside the Pod (ADR-0061 s3-feed), not on the control plane.
    pub fn with_workspace_cas(mut self, cas: Arc<dyn Cas>) -> Self {
        self.workspace_cas = Some(cas);
        self
    }

    /// Point Step Pods at the workspace service for their inputs (ADR-0061
    /// s3-feed): the `scarab-workspace-init` container becomes the Scarab-owned
    /// fetcher, and no workspace bytes cross the Kubernetes API server.
    pub fn with_workspace_service(mut self, fetch: WorkspaceFetch) -> Self {
        self.workspace_fetch = Some(fetch);
        self
    }

    /// Wire the Depot drain handle (ADR-0064 control-plane half): the drain
    /// then ingests WARM-first through `depot` (one walk, `/have`-dedup),
    /// prunes against warm-write/tiered-read, and awaits
    /// `depot.flush(published_root)` — the durability gate — before annotating
    /// the Pod and releasing the sidecar. Without this the drain writes
    /// `workspace_cas` directly, which is only correct when that handle IS the
    /// durable store (no workspace service configured).
    pub fn with_workspace_depot(
        mut self,
        depot: Arc<scarab_workspace_client::WorkspaceClient>,
    ) -> Self {
        self.workspace_depot = Some(depot);
        self
    }

    /// Enable result capture via the per-Pod egress sidecar (ADR-0042).
    pub fn with_results_egress(mut self, egress: ResultsEgress) -> Self {
        self.results_egress = Some(egress);
        self
    }

    /// Override the global default step deadline (ADR-0047).
    pub fn with_default_step_timeout_secs(mut self, secs: u32) -> Self {
        self.default_step_timeout_secs = secs;
        self
    }

    /// Connect a client from the ambient kube config (respects `KUBECONFIG`) —
    /// the dev harness points that at its kind cluster, never a prod context.
    pub async fn connect(namespace: impl Into<String>) -> Result<Self, ExecError> {
        let client = kube::Client::try_default()
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?;
        Ok(Self::with_client(namespace, client))
    }

    fn pods(&self) -> Result<Api<Pod>, ExecError> {
        let client = self.client.clone().ok_or(ExecError::Unavailable)?;
        Ok(Api::namespaced(client, &self.namespace))
    }

    /// Upsert the per-Pod Secret carrying the clone credential (ADR-0045
    /// §Token delivery): mounted as tmpfs by the kubelet, read via
    /// GIT_ASKPASS — the token is never in env, URL, or argv. Owner-referenced
    /// to the Pod so it is garbage-collected with it. Re-drives refresh the
    /// short-TTL token.
    /// Upsert every per-Pod Secret a step may need before it can run: the clone
    /// token (ADR-0045), the per-attempt OIDC token (ADR-0015), and a build
    /// step's registry dockerconfig (ADR-0018). Each helper is a no-op when the
    /// step doesn't use that credential. Idempotent (create-or-replace), so it
    /// is safe on the fresh-create path, on re-drive re-attach, and after a
    /// create-409 race — every path that ends with an existing Pod must call
    /// this, or the Pod can mount a Secret that never gets created (FailedMount).
    async fn ensure_step_secrets(
        &self,
        pod_name: &str,
        pod: &Pod,
        step: &StepRun,
        spec: &StepSpec,
    ) -> Result<(), ExecError> {
        self.ensure_clone_secret(pod_name, pod, spec).await?;
        self.ensure_registry_secret(pod_name, pod, spec).await?;
        self.ensure_oidc_secret(pod_name, pod, spec).await?;
        self.ensure_workspace_secret(pod_name, pod, step, spec)
            .await?;
        Ok(())
    }

    /// Upsert the per-Pod Secret carrying the **workspace token** (ADR-0061):
    /// mounted read-only on tmpfs at [`workspace_token::WORKSPACE_SECRETS_MOUNT_PATH`],
    /// pointed at by `SCARAB_WORKSPACE_TOKEN_FILE`, and held only by the fetcher
    /// init container — never by the untrusted step container.
    ///
    /// Re-minted on every call, exactly like the clone credential, because the
    /// token carries an **expiry**: a Pod adopted by a re-drive twenty minutes
    /// later must not present a token that expired while it was Pending.
    /// [`workspace_token::mint`] is deterministic in its claims, so a re-mint at
    /// the same instant is byte-identical and the replace is a no-op.
    async fn ensure_workspace_secret(
        &self,
        pod_name: &str,
        pod: &Pod,
        step: &StepRun,
        spec: &StepSpec,
    ) -> Result<(), ExecError> {
        // No inputs ⇒ no fetch ⇒ no credential. Not "an empty token": a Secret
        // that exists is a Secret something might mount.
        if self.workspace_cas.is_none() || spec.workspace_inputs.is_empty() {
            return Ok(());
        }
        let Some(fetch) = &self.workspace_fetch else {
            return Ok(());
        };
        let token = self.mint_workspace_token(fetch, step, spec);
        self.put_workspace_token_secret(pod_name, Some(pod), token)
            .await
    }

    /// Create-or-replace the per-Pod workspace-token Secret. Shared by the step
    /// path and the debug Pod so there is exactly one place that decides the
    /// Secret's name, key and owner reference.
    async fn put_workspace_token_secret(
        &self,
        pod_name: &str,
        pod: Option<&Pod>,
        token: String,
    ) -> Result<(), ExecError> {
        let client = self.client.clone().ok_or(ExecError::Unavailable)?;
        let secrets: Api<k8s_openapi::api::core::v1::Secret> =
            Api::namespaced(client, &self.namespace);
        let secret = k8s_openapi::api::core::v1::Secret {
            metadata: ObjectMeta {
                name: Some(workspace_secret_name(pod_name)),
                namespace: Some(self.namespace.clone()),
                owner_references: pod.and_then(|p| p.metadata.uid.clone()).map(|uid| {
                    vec![
                        k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                            api_version: "v1".into(),
                            kind: "Pod".into(),
                            name: pod_name.to_string(),
                            uid,
                            ..Default::default()
                        },
                    ]
                }),
                ..Default::default()
            },
            string_data: Some(std::collections::BTreeMap::from([(
                workspace_token::WORKSPACE_TOKEN_KEY.to_string(),
                token,
            )])),
            ..Default::default()
        };
        match secrets.create(&PostParams::default(), &secret).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 409 => secrets
                .replace(
                    &workspace_secret_name(pod_name),
                    &PostParams::default(),
                    &secret,
                )
                .await
                .map(|_| ())
                .map_err(|e| ExecError::Launch(format!("workspace secret: {e}"))),
            Err(e) => Err(ExecError::Launch(format!("workspace secret: {e}"))),
        }
    }

    /// The debug Pod's workspace token: the same codec, fenced to the step being
    /// reproduced with attempt `debug`, and scoped to the ONE snapshot root it
    /// re-materialises. Not [`Scope::Browse`](workspace_token::Scope::Browse) —
    /// browse tokens are minted by the control plane for itself and are not root
    /// limited, and a debug Pod is a Pod.
    fn mint_debug_workspace_token(
        &self,
        fetch: &WorkspaceFetch,
        step: &StepRun,
        root: &str,
        ttl_secs: u64,
    ) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let exp = workspace_token::expiry_for(now, u32::try_from(ttl_secs).unwrap_or(u32::MAX));
        let claims = workspace_token::step_claims(
            workspace_token::Fence {
                run: step.run.0.clone(),
                step: step.step.0.clone(),
                attempt: "debug".to_string(),
            },
            exp,
            vec![root.to_string()],
        );
        workspace_token::mint(&fetch.token_secret, &claims)
    }

    /// Mint the fence-scoped, expiring, root-limited workspace token for a Step.
    ///
    /// `roots` is exactly the Step's declared inputs, so the service can refuse a
    /// tree read the Step was never given (`WorkspaceClaims::may_read_tree`) — a
    /// compromised Step cannot enumerate other runs' snapshots by asking.
    fn mint_workspace_token(
        &self,
        fetch: &WorkspaceFetch,
        step: &StepRun,
        spec: &StepSpec,
    ) -> String {
        let attempt = step
            .current_attempt()
            .map(|a| a.id.0.clone())
            .unwrap_or_else(|| "0".to_string());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let exp = workspace_token::expiry_for(
            now,
            spec.timeout_seconds.unwrap_or(self.default_step_timeout_secs),
        );
        let claims = workspace_token::step_claims(
            workspace_token::Fence {
                run: step.run.0.clone(),
                step: step.step.0.clone(),
                attempt,
            },
            exp,
            spec.workspace_inputs.clone(),
        );
        workspace_token::mint(&fetch.token_secret, &claims)
    }

    async fn ensure_clone_secret(
        &self,
        pod_name: &str,
        pod: &Pod,
        spec: &StepSpec,
    ) -> Result<(), ExecError> {
        let Some(cred) = spec.clone.as_ref().and_then(|c| c.credential.as_ref()) else {
            return Ok(());
        };
        let client = self.client.clone().ok_or(ExecError::Unavailable)?;
        let secrets: Api<k8s_openapi::api::core::v1::Secret> =
            Api::namespaced(client, &self.namespace);
        let secret = k8s_openapi::api::core::v1::Secret {
            metadata: ObjectMeta {
                name: Some(clone_secret_name(pod_name)),
                namespace: Some(self.namespace.clone()),
                owner_references: pod.metadata.uid.clone().map(|uid| {
                    vec![
                        k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                            api_version: "v1".into(),
                            kind: "Pod".into(),
                            name: pod_name.to_string(),
                            uid,
                            ..Default::default()
                        },
                    ]
                }),
                ..Default::default()
            },
            string_data: Some(std::collections::BTreeMap::from([(
                CLONE_TOKEN_KEY.to_string(),
                cred.token.clone(),
            )])),
            ..Default::default()
        };
        match secrets.create(&PostParams::default(), &secret).await {
            Ok(_) => Ok(()),
            // Exists: refresh the token in place (short-TTL rotation on re-drive).
            Err(kube::Error::Api(ae)) if ae.code == 409 => secrets
                .replace(
                    &clone_secret_name(pod_name),
                    &PostParams::default(),
                    &secret,
                )
                .await
                .map(|_| ())
                .map_err(|e| ExecError::Launch(format!("clone secret: {e}"))),
            Err(e) => Err(ExecError::Launch(format!("clone secret: {e}"))),
        }
    }

    /// Upsert the per-Pod Secret carrying a build step's registry
    /// dockerconfigjson (ADR-0018): mounted read-only as `DOCKER_CONFIG`
    /// (tmpfs) — the credential is never in env, argv, or the stored spec.
    /// Owner-referenced to the Pod; re-drives refresh it.
    async fn ensure_registry_secret(
        &self,
        pod_name: &str,
        pod: &Pod,
        spec: &StepSpec,
    ) -> Result<(), ExecError> {
        let Some(config_json) = spec.build.as_ref().and_then(registry_dockerconfig) else {
            return Ok(());
        };
        let client = self.client.clone().ok_or(ExecError::Unavailable)?;
        let secrets: Api<k8s_openapi::api::core::v1::Secret> =
            Api::namespaced(client, &self.namespace);
        let secret = k8s_openapi::api::core::v1::Secret {
            metadata: ObjectMeta {
                name: Some(registry_secret_name(pod_name)),
                namespace: Some(self.namespace.clone()),
                owner_references: pod.metadata.uid.clone().map(|uid| {
                    vec![
                        k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                            api_version: "v1".into(),
                            kind: "Pod".into(),
                            name: pod_name.to_string(),
                            uid,
                            ..Default::default()
                        },
                    ]
                }),
                ..Default::default()
            },
            string_data: Some(std::collections::BTreeMap::from([(
                REGISTRY_AUTH_KEY.to_string(),
                config_json,
            )])),
            ..Default::default()
        };
        match secrets.create(&PostParams::default(), &secret).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 409 => secrets
                .replace(
                    &registry_secret_name(pod_name),
                    &PostParams::default(),
                    &secret,
                )
                .await
                .map(|_| ())
                .map_err(|e| ExecError::Launch(format!("registry secret: {e}"))),
            Err(e) => Err(ExecError::Launch(format!("registry secret: {e}"))),
        }
    }

    /// Upsert the per-Pod Secret carrying the per-attempt OIDC token
    /// (ADR-0015): mounted read-only on tmpfs, pointed at by
    /// `SCARAB_OIDC_TOKEN_FILE`. Owner-referenced to the Pod; re-drives
    /// refresh the short-lived token.
    async fn ensure_oidc_secret(
        &self,
        pod_name: &str,
        pod: &Pod,
        spec: &StepSpec,
    ) -> Result<(), ExecError> {
        let Some(token) = &spec.oidc_token else {
            return Ok(());
        };
        let client = self.client.clone().ok_or(ExecError::Unavailable)?;
        let secrets: Api<k8s_openapi::api::core::v1::Secret> =
            Api::namespaced(client, &self.namespace);
        let secret = k8s_openapi::api::core::v1::Secret {
            metadata: ObjectMeta {
                name: Some(oidc_secret_name(pod_name)),
                namespace: Some(self.namespace.clone()),
                owner_references: pod.metadata.uid.clone().map(|uid| {
                    vec![
                        k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                            api_version: "v1".into(),
                            kind: "Pod".into(),
                            name: pod_name.to_string(),
                            uid,
                            ..Default::default()
                        },
                    ]
                }),
                ..Default::default()
            },
            string_data: Some(std::collections::BTreeMap::from([(
                OIDC_TOKEN_KEY.to_string(),
                token.clone(),
            )])),
            ..Default::default()
        };
        match secrets.create(&PostParams::default(), &secret).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 409 => secrets
                .replace(&oidc_secret_name(pod_name), &PostParams::default(), &secret)
                .await
                .map(|_| ())
                .map_err(|e| ExecError::Launch(format!("oidc secret: {e}"))),
            Err(e) => Err(ExecError::Launch(format!("oidc secret: {e}"))),
        }
    }

    /// Drive the workspace lifecycle for `pod` (ADR-0029/0045/0061). **One**
    /// idempotent leg now, all state derived from the Pod itself:
    ///
    /// **Snapshot (drain)** — once the step container has terminated and the
    /// egress sidecar is still holding the Pod open, tar `/workspace` out
    /// (successful steps only), ingest it WARM-first into the Depot (per-file
    /// merkle dedup — unchanged blobs upload nothing), prune to the authored
    /// `outputs:`, await `flush(published_root)` — the ADR-0064 durability
    /// gate: the Depot uploads the closure to cold and only `Durable` lets
    /// this leg proceed — THEN patch the root onto the Pod as an annotation,
    /// harvest the artifacts of record (EVERY terminated step, whatever its
    /// exit code — a28a173), and release the sidecar.
    ///
    /// # The feed leg used to be here, and is gone (ADR-0061 s3-feed)
    ///
    /// It materialised every input into a control-plane tempdir, tarred it, and
    /// streamed the tar into a `busybox` doorstop over `kubectl exec`. The
    /// fetcher init container ([`WorkspaceFetch`]) does it inside the Pod now, so
    /// this function's only remaining involvement in the feed is *nothing* —
    /// there is no marker to touch and no barrier to release. Three things
    /// improved, and only one of them is speed:
    ///
    /// - bulk data no longer crosses the **API server** (structural);
    /// - the control plane no longer writes then re-reads the whole workspace
    ///   through its page cache — s2 measured that tmpdir round-trip *growing* to
    ///   ~4.7 s as the CAS leg got faster, so it had to be removed rather than
    ///   accelerated;
    /// - a **partial** tree can no longer be published on the FEED side (git-bug
    ///   `a3e7845`): a failed fetch fails the init container and the Step never
    ///   runs. The drain side had the same hole and it was NOT closed by deleting
    ///   the feed — see below.
    ///
    /// s0 priced the deleted tunnel at 4–15% of a Step boundary, so this is not
    /// sold as a performance win. It is the prerequisite for lazy materialisation,
    /// which is.
    ///
    /// The **drain** tunnel (and the third `exec` tar inside
    /// [`harvest_artifacts`](Self::harvest_artifacts)) are still here on purpose:
    /// s3-drain is a separately-ticketed slice (git-bug `7f05f39`). What they are
    /// no longer allowed to do is publish a tree they cannot prove is whole:
    /// [`exec_capture_stdout`](Self::exec_capture_stdout) frames every captured
    /// stream and refuses an incomplete one, so a `tar` cut short is a withheld
    /// verdict rather than a green Attempt over a partial snapshot.
    async fn drive_workspace(
        &self,
        pods: &Api<Pod>,
        pod: &Pod,
        cas: &Arc<dyn Cas>,
    ) -> Result<(), DriveErr> {
        let name = pod.metadata.name.clone().ok_or("pod has no name")?;
        let annotations = pod.metadata.annotations.clone().unwrap_or_default();
        // No "is this a workspace Pod?" guard here any more, and that is the
        // untangling git-bug 7f05f39 asked for. It used to be
        // `annotations.contains(ANNOTATION_WS_INPUTS)`, which conflated two
        // different questions — "is this a workspace Pod" (annotation *present*,
        // inserted even when empty) and "does it need a feed" (annotation
        // *non-empty*). The drain leg below already asks the only question that
        // matters, and asks it of the Pod's own containers rather than of an
        // annotation: a non-workspace Pod has no egress sidecar, so every branch
        // below is a no-op for it. `ANNOTATION_WS_INPUTS` is now written only
        // when there ARE inputs, and is evidence rather than control flow.

        // --- The one leg: snapshot after the step exits. --------------------
        let step_exit = step_terminated_exit(pod);
        if let Some(exit) = step_exit {
            if init_container_running(pod, WORKSPACE_EGRESS_CONTAINER) {
                let already = annotations
                    .get(ANNOTATION_WS_ROOT)
                    .is_some_and(|v| !v.is_empty());
                if exit == 0 && !already {
                    // ADR-0061 s0: the other half of the Step boundary — the
                    // `exec` drain, the server-side unpack, and the ingest
                    // (hash + store, inseparable from out here: `ingest` hashes
                    // and does its `/have`/`put` per blob inside the CAS impl).
                    let t_leg = std::time::Instant::now();
                    let out = self
                        .exec_capture_stdout(
                            pods,
                            &name,
                            WORKSPACE_EGRESS_CONTAINER,
                            &format!("tar -cf - -C {WORKSPACE_MOUNT_PATH} ."),
                        )
                        .await?;
                    let exec_drain_ms = t_leg.elapsed().as_millis();
                    let drain_tar_bytes = out.len() as u64;
                    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
                    let t_unpack = std::time::Instant::now();
                    unpack_dir(&out, tmp.path())?;
                    let tar_unpack_ms = t_unpack.elapsed().as_millis();
                    let (files, tree_bytes, walk_ms) = dir_stats(tmp.path());
                    // ADR-0064 (control-plane half): the drain is WARM-FIRST.
                    // With a Depot wired, `ingest` lands the snapshot on the
                    // Depot only — one walk, `/have`-dedup — and durability is
                    // the separate, awaited `flush` below. Without one, the
                    // target IS the durable store and writes need no flush.
                    let drain: Arc<dyn Cas> = match &self.workspace_depot {
                        Some(depot) => Arc::new(DrainCas {
                            warm: depot.clone(),
                            read: cas.clone(),
                        }),
                        None => cas.clone(),
                    };
                    let t_ingest = std::time::Instant::now();
                    let snapshot = match drain
                        .ingest(tmp.path().to_str().ok_or("tmp path")?)
                        .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            return Err(self
                                .drain_failure(
                                    pods,
                                    &name,
                                    &annotations,
                                    false,
                                    format!("ingest: {e}"),
                                )
                                .await)
                        }
                    };
                    let cas_ingest_ms = t_ingest.elapsed().as_millis();
                    // Per-path publishing (ADR-0007): restrict the published root
                    // to the authored `outputs:` paths. The whole workspace is
                    // still ingested first — blobs are shared, so pruning is a
                    // tree rebuild that uploads nothing new. When a prune
                    // happens, the UNPRUNED full snapshot is deliberately never
                    // flushed: it lands warm-only, so it is an evictable cache,
                    // NOT durable evidence — do not lean on "recoverable from
                    // the full snapshot" reasoning anywhere downstream
                    // (ADR-0064; only the published root's closure is
                    // guaranteed). A declared path the step did not produce is a
                    // permanent contract violation, never a narrower publish.
                    let declared: Vec<String> = annotations
                        .get(ANNOTATION_WS_OUTPUTS)
                        .map(|csv| {
                            csv.split(',')
                                .map(str::trim)
                                .filter(|p| !p.is_empty())
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    let t_prune = std::time::Instant::now();
                    // Two coordinates leave this leg (ADR-0061 s8): the published
                    // ROOT, which is where the bytes are, and its content
                    // IDENTITY, which is what they are. `ingest` folds the
                    // identity for free; a pruned root is a different tree, so its
                    // identity has to be walked — cheap, because a pruned tree is
                    // small by construction and only trees are read, never blobs.
                    let (published, identity) = if declared.is_empty() {
                        (snapshot.root, snapshot.identity)
                    } else {
                        // Prune over the drain handle: reads fall through the
                        // tiered pair, but the prune-minted trees are WRITES and
                        // land warm only — the flush below is what makes the
                        // published closure durable (the CP twin of the Depot's
                        // own DrainCas split, ADR-0064).
                        let pruned = match scarab_storage::prune_tree(
                            drain.as_ref(),
                            &snapshot.root,
                            &declared,
                        )
                        .await
                        {
                            Ok(pruned) => pruned,
                            Err(scarab_storage::PruneError::Storage(e)) => {
                                return Err(self
                                    .drain_failure(
                                        pods,
                                        &name,
                                        &annotations,
                                        false,
                                        format!("prune outputs: {e}"),
                                    )
                                    .await)
                            }
                            Err(permanent) => {
                                return Err(DriveErr::OutputContract(format!(
                                    "outputs: {permanent} (declared: {})",
                                    declared.join(", ")
                                )))
                            }
                        };
                        let identity = match scarab_storage::content_identity(
                            drain.as_ref(),
                            &pruned,
                        )
                        .await
                        {
                            Ok(identity) => identity,
                            Err(e) => {
                                return Err(self
                                    .drain_failure(
                                        pods,
                                        &name,
                                        &annotations,
                                        false,
                                        format!("outputs identity: {e}"),
                                    )
                                    .await)
                            }
                        };
                        (pruned, Some(identity))
                    };
                    let cas_prune_ms = t_prune.elapsed().as_millis();
                    // The durability gate (ADR-0061 part 4 as amended by
                    // ADR-0064): durability is not local, so the batched cold
                    // upload stays ON the critical path — the Depot walks the
                    // PUBLISHED root's closure and uploads what cold is
                    // missing. Two outcomes release the verdict: `Durable`
                    // (cold holds the closure) and `WarmOnly` (the Depot HAS
                    // no cold tier — a deployment posture, not a failure; see
                    // the arm below). Failures are classified by
                    // `drain_failure`: `Retry` and transport errors re-drive,
                    // time-bounded; `Fatal` is EvidenceLost now (4cf03d7).
                    let t_flush = std::time::Instant::now();
                    // ADR-0064 s2: what the flush EARNED, for the durability
                    // stamp below. `Durable` and `WarmOnly` both proceed to
                    // the annotation patch — a warm-only landing is a Depot
                    // posture (no cold tier configured), not a failure, and
                    // withholding the verdict for it would make "no object
                    // store" mean "no green ever".
                    let durability: Option<String> = if let Some(depot) = &self.workspace_depot {
                        use scarab_workspace_client::FlushOutcome;
                        match depot.flush(&published).await {
                            outcome @ FlushOutcome::Durable { .. } => {
                                durability_stamp(&outcome).map(str::to_string)
                            }
                            outcome @ FlushOutcome::WarmOnly => {
                                // Logged once per Attempt, not once per poll:
                                // the `already` root-annotation guard at the
                                // top of this leg makes a successfully
                                // patched drain once-only.
                                tracing::info!(
                                    pod = %name,
                                    root = %published.0,
                                    "workspace flush landed warm-only: the Depot has no cold \
                                     tier behind it, so the verdict is released on the warm \
                                     copy (ADR-0064 s2)"
                                );
                                durability_stamp(&outcome).map(str::to_string)
                            }
                            FlushOutcome::Retry(cause) => {
                                return Err(self
                                    .drain_failure(
                                        pods,
                                        &name,
                                        &annotations,
                                        false,
                                        format!("flush: {cause}"),
                                    )
                                    .await)
                            }
                            FlushOutcome::Fatal(cause) => {
                                return Err(self
                                    .drain_failure(
                                        pods,
                                        &name,
                                        &annotations,
                                        true,
                                        format!("flush: {cause}"),
                                    )
                                    .await)
                            }
                        }
                    } else {
                        // No Depot: the drain above wrote the CP store — the
                        // durable store itself — directly, and the durability
                        // stamp stays un-patched (NULL). Approved deferral
                        // (ADR-0064 s2): `output_durability` reports `None`.
                        None
                    };
                    let cold_flush_ms = t_flush.elapsed().as_millis();
                    // Record all of these on the Pod BEFORE releasing the
                    // sidecar: output()/output_identity()/output_durability()
                    // read them durably across control-plane restarts. One
                    // patch, so a crash between them cannot leave a root whose
                    // content nobody can compare — or whose durability tier
                    // nobody can audit (same crash-atomicity argument,
                    // ADR-0064 s2). Ordering with the flush above is
                    // load-bearing: the root annotation is the durable claim
                    // `output()` reports, so it may only exist once the
                    // closure's durability has been settled.
                    let mut annotation_patch = serde_json::Map::new();
                    annotation_patch.insert(
                        ANNOTATION_WS_ROOT.to_string(),
                        serde_json::Value::String(published.0.clone()),
                    );
                    annotation_patch.insert(
                        ANNOTATION_WS_IDENTITY.to_string(),
                        serde_json::Value::String(
                            identity.map(|i| i.0).unwrap_or_default(),
                        ),
                    );
                    // `Durable { tier: None }` (old-Depot skew window) and the
                    // no-Depot branch both stamp NOTHING for this key, so the
                    // annotation stays absent rather than lying with a guess.
                    if let Some(tier) = durability {
                        annotation_patch.insert(
                            ANNOTATION_WS_DURABILITY.to_string(),
                            serde_json::Value::String(tier),
                        );
                    }
                    let patch = serde_json::json!({
                        "metadata": { "annotations": annotation_patch }
                    });
                    pods.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                        .await
                        .map_err(|e| format!("annotate root: {e}"))?;
                    // The drain made it: whatever transient laps preceded it,
                    // the escalation clock is over — drop the in-process
                    // fallback anchor so the map only ever holds live fences.
                    self.forget_drain_anchor(&name);
                    tracing::info!(
                        pod = %name,
                        leg = "drain",
                        files,
                        tree_bytes,
                        tar_bytes = drain_tar_bytes,
                        exec_drain_ms,
                        tar_unpack_ms,
                        walk_ms,
                        cas_ingest_ms,
                        cas_prune_ms,
                        cold_flush_ms,
                        outputs = declared.len(),
                        total_ms = t_leg.elapsed().as_millis(),
                        "ws-timing"
                    );
                }
                // Harvest artifacts of record (ADR-0052): everything the
                // step wrote to /scarab/artifacts, filtered by its authored
                // globs, uploaded as plain object blobs (NOT the CAS — an
                // independent lifecycle) and indexed on the Pod annotation.
                //
                // Unlike the snapshot above this runs for EVERY terminated step,
                // whatever its exit code (a28a173). A failing step's artifacts
                // are the ones a human most wants — the JUnit XML of the suite
                // that just went red, the crash log, the screenshot — and the
                // scheduler has always had an `ExecState::Failed` harvest branch
                // waiting for them ("often THE evidence", ADR-0056). Gating this
                // on `exit == 0` made that branch dead code on k8s and threw the
                // evidence away.
                //
                // The upload is a real, already-committed effect, so its INDEX
                // must be equally durable: a harvest error is TRANSIENT, exactly
                // like the workspace snapshot above, and returns before the
                // sidecar is released. That keeps the settle barrier closed, so
                // the next poll re-harvests (idempotent: same object keys, and
                // the annotation guard makes a completed harvest once-only) and
                // the terminal verdict — Succeeded (98ea804) or the step's own
                // Failed (a28a173) — stays withheld meanwhile (see `poll`).
                // Swallowing the error instead released the barrier and reported
                // a verdict with the blobs uploaded and NOTHING indexed.
                // Retries are bounded by the engine's step-timeout backstop.
                if let Some(store) = &self.artifact_store {
                    if artifact_harvest_owed(pod, true) {
                        if let Err(e) = self
                            .harvest_artifacts(pods, pod, &name, store.as_ref())
                            .await
                        {
                            // A harvest error is EVIDENCE, never a verdict
                            // (a28a173). It is only ever `Transient` — never
                            // `OutputContract`/`InputMissing` — so it can never
                            // be reported as `Failed { class: Config | Infra }`
                            // and displace the step's own failure. The class is
                            // re-derived from the Pod's exit code on every poll,
                            // so a broken harvest can delay the report; it can
                            // never change what the step is reported to have done.
                            eprintln!(
                                "scarab-executor: artifact harvest failed for pod {name} \
                                 (step exit {exit}) — the step's own verdict is unchanged: {e}"
                            );
                            return Err(DriveErr::Transient(format!("artifact harvest: {e}")));
                        }
                    }
                }
                // Release the sidecar (idempotent): failed steps snapshot
                // nothing — their workspace is not an output (their artifacts,
                // harvested above, are).
                self.release_egress_sidecar(pods, &name).await?;
            }
        }
        Ok(())
    }

    /// Release the workspace egress sidecar: touch the marker its wait-loop —
    /// which deliberately ignores SIGTERM — is blocked on, so the container
    /// (and with it the Pod) can terminate. This is the ONE spelling of the
    /// release (ticket e10cf7e): every path that is done with a workspace Pod
    /// routes through here — `drive_workspace` after a settle, and `poll`'s
    /// terminal `OutputContract`/`EvidenceLost` arms, which bail out of
    /// `drive_workspace` BEFORE its release and used to strand the sidecar in
    /// its loop forever (Pod phase-Running, holding node resources and its
    /// emptyDir, matched by no reaper). Idempotent: touching an existing
    /// marker is a no-op, and a release that never lands leaves the sidecar
    /// running for the next poll to re-issue.
    async fn release_egress_sidecar(&self, pods: &Api<Pod>, name: &str) -> Result<(), String> {
        self.exec_with_stdin(
            pods,
            name,
            WORKSPACE_EGRESS_CONTAINER,
            &egress_release_cmd(),
            Vec::new(),
        )
        .await
    }

    /// Classify a failed Depot drain leg (ingest / prune / flush) and keep the
    /// escalation clock (ADR-0064 / ticket 4cf03d7). The decision itself is
    /// the pure [`drain_failure_verdict`]; this wrapper supplies its inputs —
    /// reading, and on the FIRST failure recording, the
    /// [`ANNOTATION_WS_DRAIN_FIRST_FAILURE`] anchor on the Pod (durable across
    /// control-plane restarts, so the outage clock neither resets nor is
    /// lost).
    ///
    /// ONLY these drain legs route here. Artifact-harvest failures keep their
    /// plain-`Transient` loop: an artifact-store blip must never escalate a
    /// step whose evidence IS durable into `EvidenceLost`.
    ///
    /// On a drain that eventually succeeds, the annotation is simply left
    /// behind: the root annotation gates re-entry to the drain leg, so no
    /// later poll ever consults the anchor again — stale-but-inert, and no
    /// cleanup patch is owed. (The IN-MEMORY fallback anchor, by contrast, IS
    /// cleaned up on success — see the end of the drain leg — because nothing
    /// garbage-collects process memory with the Pod.)
    async fn drain_failure(
        &self,
        pods: &Api<Pod>,
        name: &str,
        annotations: &std::collections::BTreeMap<String, String>,
        fatal: bool,
        cause: String,
    ) -> DriveErr {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if fatal {
            // Escalating now — the Attempt goes terminal, so the fallback
            // anchor (if any polls seeded one) is dead weight: drop it.
            self.forget_drain_anchor(name);
            return DriveErr::EvidenceLost { cause };
        }
        // The annotation anchor comes FIRST (durable with the Pod across
        // control-plane restarts — authoritative whenever it is readable /
        // writable). Idempotent write, only if absent: a later failed drive
        // must observe the ORIGINAL clock, never restart it.
        let annotation_anchor: Result<i64, String> = match annotations
            .get(ANNOTATION_WS_DRAIN_FIRST_FAILURE)
            .and_then(|v| v.parse::<i64>().ok())
        {
            Some(first) => Ok(first),
            None => {
                let patch = serde_json::json!({
                    "metadata": { "annotations": {
                        ANNOTATION_WS_DRAIN_FIRST_FAILURE: now_ms.to_string(),
                    } }
                });
                match pods
                    .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
                    .await
                {
                    Ok(_) => Ok(now_ms),
                    // The anchor could be neither read (absent) nor written.
                    // One blip here is the same API-server weather as the
                    // drain failure itself and must not panic or force a
                    // premature verdict — but if this write fails on EVERY
                    // poll (revoked patch RBAC, broken admission webhook),
                    // "retry the write next poll" means the anchor NEVER
                    // exists and the verdict stays Transient until the
                    // step-budget timeout — the 4cf03d7 disguise. So the
                    // clock falls back to the in-process anchor below.
                    Err(e) => Err(e.to_string()),
                }
            }
        };
        let (first, anchor_err) = {
            let mut anchors = self
                .ws_drain_fallback_anchors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            select_drain_anchor(annotation_anchor, &mut anchors, name, now_ms)
        };
        if let Some(e) = &anchor_err {
            eprintln!(
                "scarab-executor: could not record drain first-failure on pod \
                 {name} (using in-process fallback clock): {e}"
            );
        }
        match drain_failure_verdict(false, Some(first), now_ms) {
            DrainVerdict::Transient => DriveErr::Transient(cause),
            DrainVerdict::EvidenceLost => {
                self.forget_drain_anchor(name);
                DriveErr::EvidenceLost {
                    cause: evidence_lost_cause(cause, anchor_err),
                }
            }
        }
    }

    /// Drop the in-process fallback anchor for a fence (keyed by its Pod
    /// name): called when its drain succeeds or its Attempt escalates to
    /// [`DriveErr::EvidenceLost`] — the two ways a fence's escalation clock
    /// ends. (An `OutputContract` exit can strand an entry; the 2×-window
    /// prune in [`select_drain_anchor`] mops those up — it runs on EVERY
    /// `drain_failure`, the annotation path included, so strays are collected
    /// even after RBAC heals and fallback inserts stop.)
    fn forget_drain_anchor(&self, name: &str) {
        self.ws_drain_fallback_anchors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(name);
    }

    /// Tar `/scarab/artifacts` out of the egress container, filter by the
    /// step's authored globs, upload each file as `artifacts/{run}/{name}`,
    /// and record the metadata on the Pod (ADR-0052).
    async fn harvest_artifacts(
        &self,
        pods: &Api<Pod>,
        pod: &Pod,
        name: &str,
        store: &dyn scarab_storage::ObjectStore,
    ) -> Result<(), String> {
        let run = pod
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("scarab.io/run"))
            .cloned()
            .unwrap_or_default();
        let globs: Vec<String> = pod
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(ANNOTATION_ARTIFACT_GLOBS))
            .and_then(|v| serde_json::from_str(v).ok())
            .unwrap_or_default();
        // ADR-0061 s0: the artifact harvest is a THIRD `exec` tar tunnel on every
        // Step boundary (it runs for every terminated step, any exit code), so it
        // belongs in the same budget as the workspace legs.
        let t_leg = std::time::Instant::now();
        // `|| true` used to swallow tar's exit status here — it existed for the
        // "no artifacts directory" case, and paid for it by making a tar that died
        // mid-stream indistinguishable from one that had nothing to say. The
        // absent-directory case is now asked explicitly, so the failure of a tar
        // that DID run propagates (git-bug `a3e7845`); an `if` with no `else` exits
        // 0, so the sentinel still lands when there is nothing to harvest.
        let out = self
            .exec_capture_stdout(
                pods,
                name,
                WORKSPACE_EGRESS_CONTAINER,
                &format!(
                    "if [ -d {ARTIFACTS_MOUNT_PATH} ]; \
                     then tar -cf - -C {ARTIFACTS_MOUNT_PATH} . 2>/dev/null; fi"
                ),
            )
            .await?;
        let exec_drain_ms = t_leg.elapsed().as_millis();
        let drain_tar_bytes = out.len() as u64;
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        if !out.is_empty() {
            // …and the unpack error propagates too. It was `let _ =`, whose comment
            // ("an empty dir tars to nothing") describes a case the `is_empty`
            // guard above already handles — so the discard only ever hid a real
            // corrupt/truncated archive, and hid it in the leg that indexes a
            // failed step's ONLY evidence. A harvest error is transient by
            // construction (see the call site), so propagating it delays the
            // verdict; discarding it published an empty index as the truth.
            unpack_dir(&out, tmp.path())?;
        }

        let t_upload = std::time::Instant::now();
        let mut uploaded_bytes: u64 = 0;
        let mut metas: Vec<scarab_engine::ArtifactMeta> = Vec::new();
        let mut stack = vec![tmp.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
            for entry in entries {
                let entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path
                    .strip_prefix(tmp.path())
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned();
                if !globs.is_empty() && !globs.iter().any(|g| glob_match(g, &rel)) {
                    continue;
                }
                // An unreadable entry (a broken symlink the step left behind) is
                // PERMANENT and this file's problem alone: skip it loudly rather
                // than fail the whole harvest, which — now that a harvest error
                // holds the settle barrier — would wedge the step until its
                // timeout over one junk file. Nothing was uploaded for it, so
                // the uploaded-implies-indexed invariant is untouched.
                let bytes = match std::fs::read(&path) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        eprintln!("scarab-executor: skipping unreadable artifact {rel}: {e}");
                        continue;
                    }
                };
                let key = format!("artifacts/{run}/{rel}");
                store
                    .put(&key, bytes.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                uploaded_bytes += bytes.len() as u64;
                metas.push(scarab_engine::ArtifactMeta {
                    name: rel.clone(),
                    size: bytes.len() as u64,
                    content_type: content_type_of(&rel).to_string(),
                    object_key: key,
                });
            }
        }
        let store_upload_ms = t_upload.elapsed().as_millis();
        metas.sort_by(|a, b| a.name.cmp(&b.name));

        // Record BEFORE the sidecar releases — artifacts() reads it durably.
        let patch = serde_json::json!({
            "metadata": { "annotations": {
                ANNOTATION_ARTIFACTS: serde_json::to_string(&metas).unwrap_or_default()
            } }
        });
        pods.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map_err(|e| format!("annotate artifacts: {e}"))?;
        tracing::info!(
            pod = %name,
            leg = "artifacts",
            files = metas.len(),
            tar_bytes = drain_tar_bytes,
            uploaded_bytes,
            exec_drain_ms,
            store_upload_ms,
            total_ms = t_leg.elapsed().as_millis(),
            "ws-timing"
        );
        Ok(())
    }

    /// Run `sh -c cmd` in `container`, streaming `stdin_bytes` to it.
    async fn exec_with_stdin(
        &self,
        pods: &Api<Pod>,
        pod: &str,
        container: &str,
        cmd: &str,
        stdin_bytes: Vec<u8>,
    ) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let params = AttachParams::default()
            .container(container)
            .stdin(true)
            .stdout(true)
            .stderr(true);
        let mut proc = pods
            .exec(pod, ["sh", "-c", cmd], &params)
            .await
            .map_err(|e| format!("exec in {container}: {e}"))?;
        if let Some(mut stdin) = proc.stdin() {
            stdin
                .write_all(&stdin_bytes)
                .await
                .map_err(|e| e.to_string())?;
            stdin.shutdown().await.ok();
            drop(stdin);
        }
        // Best-effort join, but no longer SILENT. When this exec's command releases
        // a wait-loop (touching a marker), the container exits immediately and the
        // exec's status frame is torn down with it — a benign race, so an error
        // here must not be a verdict. What makes tolerating it safe is that this
        // call carries no DATA: its whole effect is one `touch`, the state machine
        // is derived from the Pod, and a release that did not happen leaves the
        // egress container running so the next poll re-issues it. That is the
        // opposite of `exec_capture_stdout`, whose bytes ARE the evidence and which
        // therefore fails closed (git-bug `a3e7845`).
        //
        // It is logged because "tolerated" and "invisible" are different things: a
        // release failing every poll until the step times out used to leave no
        // trace at all naming this exec.
        if let Err(e) = proc.join().await {
            tracing::debug!(
                pod = %pod,
                container = %container,
                "exec join failed after a control command; the Pod-derived state \
                 machine will re-issue it on the next poll: {e}"
            );
        }
        Ok(())
    }

    /// Run `sh -c cmd` in `container` and capture its stdout **only if the
    /// capture is provably complete** (git-bug `a3e7845`).
    ///
    /// # Why this is not "read the stream and trust it"
    ///
    /// Both callers hand the bytes straight to [`unpack_dir`], and one of them
    /// then publishes the result as a Step's authoritative Workspace Snapshot. A
    /// `tar` that dies partway — the egress container OOM-killed, node pressure,
    /// `SIGPIPE` at a 512-byte record boundary — produces a stdout stream that is
    /// **truncated but not an error**: `tar::Archive::unpack` treats a short read
    /// as end-of-archive and unpacks a *partial tree*, `Cas::ingest` hashes it
    /// happily, and the Attempt goes green claiming a snapshot that is missing
    /// files. That is silent data loss, and the class of thing CONTEXT.md §2 says
    /// this product may not do. The tolerance for a transient `workspace: broken
    /// pipe` in the kind tier (~1-in-9, git-bug `a0b42ad`) is the evidence that
    /// the trigger is real and not theoretical.
    ///
    /// # Two guards, and why neither alone is enough
    ///
    /// 1. **The remote command's exit status.** `AttachedProcess::join()` reports
    ///    only websocket/task failures — the remote `Status` frame arrives on a
    ///    separate channel that only [`kube::api::AttachedProcess::take_status`]
    ///    can see, so the previous code could not have noticed a `tar` that
    ///    exited non-zero. Note `take_status` MUST be called before `join`, which
    ///    drops the receiver.
    /// 2. **A sentinel appended to the stream itself.** `cmd && printf SENTINEL`
    ///    means the sentinel is on stdout *after* the payload, and only if the
    ///    command succeeded. So a stream missing its sentinel is truncated (or
    ///    the command failed) — detectable even when the status frame is lost,
    ///    which is exactly the case the status check cannot cover. The nonce makes
    ///    it unforgeable by the captured content and unmistakable for a leftover
    ///    from an earlier exec.
    ///
    /// It **fails closed**: no complete capture, no `Ok`. The callers turn that
    /// into `DriveErr::Transient`, which holds the egress barrier shut, withholds
    /// the verdict, and re-drains on the next poll — a delayed verdict instead of
    /// a wrong one.
    async fn exec_capture_stdout(
        &self,
        pods: &Api<Pod>,
        pod: &str,
        container: &str,
        cmd: &str,
    ) -> Result<Vec<u8>, String> {
        use tokio::io::AsyncReadExt as _;
        let sentinel = stream_sentinel();
        let framed = framed_command(cmd, &sentinel);
        let params = AttachParams::default()
            .container(container)
            .stdout(true)
            .stderr(false);
        let mut proc = pods
            .exec(pod, ["sh", "-c", framed.as_str()], &params)
            .await
            .map_err(|e| format!("exec in {container}: {e}"))?;
        // Before `join`, which takes the status receiver away.
        let status = proc.take_status();
        let mut out = Vec::new();
        if let Some(mut stdout) = proc.stdout() {
            stdout
                .read_to_end(&mut out)
                .await
                .map_err(|e| e.to_string())?;
        }
        proc.join().await.map_err(|e| format!("exec join: {e}"))?;

        // Guard 1. A `Failure` status is decisive; an ABSENT status is not, because
        // the frame can legitimately be torn down with the container (see
        // `exec_with_stdin`) — and the sentinel below already proves completeness
        // on its own, so refusing on a missing frame would only add false reds.
        let status = match status {
            Some(fut) => fut.await,
            None => None,
        };
        if let Some(s) = &status {
            if s.status.as_deref() != Some("Success") {
                return Err(format!(
                    "exec in {container} reported failure: status={:?} reason={:?} message={:?}",
                    s.status, s.reason, s.message
                ));
            }
        }
        // Guard 2. The sentinel, which is the one that catches a stream cut short.
        strip_stream_sentinel(out, &sentinel).map_err(|e| {
            format!(
                "{e} (container {container}, remote status {:?}) — refusing to unpack a stream \
                 whose completeness cannot be established (git-bug a3e7845)",
                status.as_ref().map(|s| s.status.clone())
            )
        })
    }
}

/// A per-exec end-of-stream sentinel for [`framed_command`].
///
/// Unique per call — wall clock plus a process-wide counter — so it can be
/// neither forged by the captured bytes nor confused with an earlier exec's.
/// It is a framing marker, not a secret; unguessability is not the property
/// being bought, uniqueness is.
fn stream_sentinel() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("__scarab-eos-{nanos:x}-{seq:x}__")
}

/// Wrap `cmd` so its stdout is terminated by `sentinel` **only on success**.
///
/// `&&` is the whole mechanism: if `cmd` fails or is killed, `printf` never runs
/// and the stream has no sentinel. Trailing bytes after a tar archive are
/// harmless — a tar reader stops at the two zero blocks — but they are stripped
/// anyway by [`strip_stream_sentinel`] before anything unpacks the payload.
fn framed_command(cmd: &str, sentinel: &str) -> String {
    format!("{cmd} && printf '%s' '{sentinel}'")
}

/// Verify `out` ends with `sentinel` and return the payload without it.
///
/// The inverse of [`framed_command`], and the truncation detector: a payload that
/// does not end in its own sentinel was cut short (or its command failed), and
/// must never reach [`unpack_dir`].
fn strip_stream_sentinel(mut out: Vec<u8>, sentinel: &str) -> Result<Vec<u8>, String> {
    let marker = sentinel.as_bytes();
    if !out.ends_with(marker) {
        return Err(format!(
            "captured stream is incomplete: {} bytes, no end-of-stream sentinel",
            out.len()
        ));
    }
    out.truncate(out.len() - marker.len());
    Ok(out)
}

/// Minimal artifact glob (ADR-0052): `*` matches any run of characters
/// (including `/`); everything else is literal. Enough for `dist/*`,
/// `*.tar.gz`, `coverage/*.html`.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn inner(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&p[1..], n) || (!n.is_empty() && inner(p, &n[1..])),
            (Some(pc), Some(nc)) if pc == nc => inner(&p[1..], &n[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), name.as_bytes())
}

/// A pragmatic content type from the artifact's extension.
fn content_type_of(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or_default() {
        "html" | "htm" => "text/html",
        "txt" | "log" => "text/plain",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "gz" | "tgz" => "application/gzip",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

/// The per-Pod Secret carrying the clone credential (owner-referenced to the
/// Pod, so it is garbage-collected with it).
fn clone_secret_name(pod_name: &str) -> String {
    format!("{pod_name}-token")
}

/// Is the named (init) container currently in a `running` state?
/// Why a `drive_workspace` call failed. `Transient` is the default (a store
/// blip, an exec hiccup): `poll` surfaces it as an error and re-drives.
/// `From<String>`/`From<&str>` keep every `?` inside `drive_workspace` — which
/// all yield string errors — compiling unchanged.
///
/// There used to be an `InputMissing` variant for "a CAS input this step needs is
/// gone". It is gone with the feed leg (ADR-0061 s3-feed): the *fetcher* is the
/// thing that now discovers a missing snapshot, from inside the Pod, and it
/// reports it by exiting non-zero. `pod_state` classifies that as
/// `Infra { never_started: true }` — deliberately the identical verdict this
/// variant produced, so nothing downstream of the executor changed.
enum DriveErr {
    Transient(String),
    /// A declared `outputs:` path was not produced (or is not a legal
    /// workspace-relative path) — permanent and author-fixable, so it fails the
    /// step with a developer verdict instead of retrying (ADR-0007 fail-closed).
    OutputContract(String),
    /// The step ran (and exited 0), but its evidence could not be made durable
    /// (ADR-0064 / ticket 4cf03d7): the Depot's `flush` said `Fatal`, or the
    /// drain kept failing past [`WS_DRAIN_ESCALATION_MS`]. The workspace lives
    /// on the Pod's emptyDir and the Depot cannot take it, so there is nothing
    /// left to wait for — `poll` fails the Attempt as
    /// `Infra { never_started: false }` WITH this cause, through the normal
    /// Failed path (author-gated `retry:` applies), never as a step-budget
    /// timeout.
    EvidenceLost { cause: String },
}

impl std::fmt::Display for DriveErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriveErr::Transient(s)
            | DriveErr::OutputContract(s)
            | DriveErr::EvidenceLost { cause: s } => f.write_str(s),
        }
    }
}

/// What a failed drain leg (ingest / prune / flush against the Depot) means
/// for the Attempt right now — pure over `(fatal, first_failure_ms, now_ms)`,
/// so tests construct the interleaving instead of scheduling it.
///
/// - A `FlushOutcome::Fatal` escalates immediately: the Depot has said the
///   snapshot can never become durable as-is, so more polling is a lie.
/// - Anything transient (`FlushOutcome::Retry`, ingest/transport errors) is
///   re-driven next poll — but bounded by TIME, not by count, anchored on the
///   [`ANNOTATION_WS_DRAIN_FIRST_FAILURE`] the caller records at the first
///   failure: past [`WS_DRAIN_ESCALATION_MS`] the verdict is `EvidenceLost`
///   carrying the LAST cause. Time, not count, because poll cadence is an
///   operator tunable — a count would make the escalation window drift with
///   config, and the 4cf03d7 ruling is about wall-clock promptness.
#[derive(Debug, PartialEq, Eq)]
enum DrainVerdict {
    /// Re-drive next poll (and record the first-failure annotation if absent).
    Transient,
    /// Fail the Attempt now, with the last cause.
    EvidenceLost,
}

/// The [`ANNOTATION_WS_DURABILITY`] value a flush outcome earns (ADR-0064
/// s2), pure so the derivation is unit-testable without a Pod:
///
/// - `Durable { tier: Some(t) }` → stamp `t` (`"object"` / `"separate-volume"`,
///   verbatim off the wire — the Depot names the tier, this side records it);
/// - `WarmOnly` → stamp `"warm-only"` (the verdict was released on the warm
///   copy; un-stamping this case would make every warm-only deployment
///   indistinguishable from a pre-s2 one);
/// - `Durable { tier: None }` → `None`: an old Depot affirmed durability
///   without naming a tier (rolling-upgrade skew), and an absent stamp beats
///   a guessed one.
///
/// `Retry`/`Fatal` are unreachable from today's one caller: the flush match
/// in `drive_workspace` returns out through `drain_failure` on both before
/// any annotation patch, and only the surviving arms call this. Kept as
/// explicit arms (not a catch-all) so a new `FlushOutcome` variant is a
/// compile error here, never a silent non-stamp — but they return `None`
/// (with a debug_assert for the test suite) rather than panic: this runs on
/// the scheduler's poll path, and if a future caller ever slips a `Retry`/
/// `Fatal` through, a wrongly-skipped stamp is a recoverable audit gap while
/// a poll panic takes the whole driver down with it.
fn durability_stamp(outcome: &scarab_workspace_client::FlushOutcome) -> Option<&str> {
    use scarab_workspace_client::FlushOutcome;
    match outcome {
        FlushOutcome::Durable { tier } => tier.as_deref(),
        FlushOutcome::WarmOnly => Some("warm-only"),
        FlushOutcome::Retry(_) | FlushOutcome::Fatal(_) => {
            debug_assert!(
                false,
                "durability_stamp reached with a non-terminal flush outcome — \
                 drive_workspace must return through drain_failure on Retry/Fatal \
                 before stamping"
            );
            None
        }
    }
}

fn drain_failure_verdict(fatal: bool, first_failure_ms: Option<i64>, now_ms: i64) -> DrainVerdict {
    if fatal {
        return DrainVerdict::EvidenceLost;
    }
    match first_failure_ms {
        Some(first) if now_ms.saturating_sub(first) > WS_DRAIN_ESCALATION_MS => {
            DrainVerdict::EvidenceLost
        }
        _ => DrainVerdict::Transient,
    }
}

/// Pick the `first_failure_ms` anchor for [`drain_failure_verdict`] (ticket
/// 66c93be) — pure over the annotation outcome and the fallback map, so the
/// anchor SELECTION is testable without a Pod, and the verdict function above
/// stays byte-identical (the fallback only changes how its input is obtained).
///
/// `annotation_anchor` is the durable Pod-annotation path's outcome:
/// - `Ok(ms)` — the annotation was read, or its if-absent write just landed.
///   The annotation is authoritative: it is used verbatim, and the fallback
///   map is neither seeded nor consulted (a stale entry from an earlier RBAC
///   outage is never promoted over the durable clock — the stray prune below
///   is the ONLY way this arm touches the map).
/// - `Err(patch error)` — the annotation could be neither read nor written.
///   The clock falls back to the in-process map: insert-if-absent with
///   `now_ms` (a later failed poll must observe the ORIGINAL fallback clock,
///   never restart it), and the patch error is returned so an escalation via
///   this path can name BOTH failures in its cause.
///
/// Tradeoff, accepted: the map is process memory, so a control-plane restart
/// resets the fallback clock — which can only DELAY escalation, never
/// fabricate it. Strictly better than the pre-fix behavior (never escalating
/// at all when the annotation write fails every poll).
///
/// Unbounded-growth cap: on EVERY invocation — annotation path included —
/// entries OTHER than the current key older than 2× [`WS_DRAIN_ESCALATION_MS`]
/// are pruned. Live clocks escalate (and are removed) within 1× the window,
/// so anything past 2× is a stray from a path that never reached success or
/// escalation. Pruning used to run only on a fallback INSERT, but once RBAC
/// heals no insert ever happens again, so a stray seeded during the outage
/// survived for the process lifetime; a cheap map sweep under the same lock
/// closes that. The current key's own seed is NEVER pruned, however old — its
/// clock may legally be past the window at the moment it is read.
fn select_drain_anchor(
    annotation_anchor: Result<i64, String>,
    fallback: &mut std::collections::HashMap<String, i64>,
    key: &str,
    now_ms: i64,
) -> (i64, Option<String>) {
    fallback.retain(|k, t| {
        k == key || now_ms.saturating_sub(*t) <= 2 * WS_DRAIN_ESCALATION_MS
    });
    match annotation_anchor {
        Ok(first) => (first, None),
        Err(patch_err) => {
            let first = *fallback.entry(key.to_string()).or_insert(now_ms);
            (first, Some(patch_err))
        }
    }
}

/// The cause an escalated [`DriveErr::EvidenceLost`] carries. When the
/// escalation ran on the in-process fallback clock, the durable anchor was
/// ALSO failing the whole time — and that second failure is evidence the
/// operator needs (it points at RBAC / admission, not the Depot alone), so
/// the cause must name both.
fn evidence_lost_cause(cause: String, anchor_err: Option<String>) -> String {
    match anchor_err {
        Some(e) => format!(
            "{cause} — and the failure anchor could not be recorded on the Pod: {e}"
        ),
        None => cause,
    }
}

/// The control-plane twin of the Depot's own drain split (ADR-0064): a `Cas`
/// whose WRITES go to the warm tier (the Depot, via the workspace client) and
/// whose READS go through the tiered pair (warm, falling through to cold).
/// `prune_tree`/`content_identity` run over this during a drain, so
/// prune-minted trees land warm only — the single awaited `flush` afterwards
/// is what makes the whole published closure durable, instead of every
/// `put_tree` paying its own cold round-trip (the double-walk shape ADR-0064
/// removed).
struct DrainCas {
    warm: Arc<dyn Cas>,
    read: Arc<dyn Cas>,
}

#[async_trait]
impl Cas for DrainCas {
    async fn put_blob(&self, data: &[u8]) -> Result<scarab_storage::BlobHash, StorageError> {
        self.warm.put_blob(data).await
    }

    async fn get_blob(&self, hash: &scarab_storage::BlobHash) -> Result<Vec<u8>, StorageError> {
        self.read.get_blob(hash).await
    }

    async fn put_tree(
        &self,
        entries: Vec<scarab_storage::TreeEntry>,
    ) -> Result<scarab_storage::TreeHash, StorageError> {
        self.warm.put_tree(entries).await
    }

    async fn tree_entries(
        &self,
        hash: &scarab_storage::TreeHash,
    ) -> Result<Vec<scarab_storage::TreeEntry>, StorageError> {
        self.read.tree_entries(hash).await
    }

    async fn materialize(
        &self,
        tree: &scarab_storage::TreeHash,
        path: &str,
    ) -> Result<(), StorageError> {
        self.read.materialize(tree, path).await
    }

    async fn ingest(&self, path: &str) -> Result<scarab_storage::Snapshot, StorageError> {
        self.warm.ingest(path).await
    }
}

impl From<String> for DriveErr {
    fn from(s: String) -> Self {
        DriveErr::Transient(s)
    }
}

impl From<&str> for DriveErr {
    fn from(s: &str) -> Self {
        DriveErr::Transient(s.to_string())
    }
}

fn init_container_running(pod: &Pod, name: &str) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.init_container_statuses.as_ref())
        .into_iter()
        .flatten()
        .any(|c| c.name == name && c.state.as_ref().is_some_and(|st| st.running.is_some()))
}

/// The step container's exit code once terminated, else `None`.
fn step_terminated_exit(pod: &Pod) -> Option<i32> {
    pod.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .iter()
        .find(|c| c.name == STEP_CONTAINER)?
        .state
        .as_ref()?
        .terminated
        .as_ref()
        .map(|t| t.exit_code)
}

/// The verdict `poll` reports for a workspace Pod whose settle leg has already
/// been driven this tick — pure, so the rule is testable without a cluster.
/// `harvesting` is whether an artifact store is wired (see `artifact_harvest_owed`).
///
/// The settle (workspace snapshot + artifact harvest) runs in the egress NATIVE
/// sidecar, which does NOT gate the Pod phase: a workspace Pod reports its
/// terminal phase the instant the step container exits, BEFORE settle has
/// patched the annotations that `output()`/`artifacts()` read. The orchestrator
/// indexes those on the terminal verdict exactly once, so reporting that verdict
/// early loses them permanently. `drive_workspace` touches `egress-done` only
/// after patching, so the sidecar still running IS "settle incomplete".
///
/// Two verdicts are withheld while their settle products are still outstanding,
/// and the shapes differ because what they owe differs:
///
/// - **Succeeded** (98ea804) owes BOTH the workspace snapshot root and the
///   artifact index. The sidecar terminating is the single signal that covers
///   both, so it is withheld for the sidecar's whole lifetime.
/// - **Failed on the step's own exit code** (a28a173) owes only the artifact
///   index — a failed step's workspace is not an output — so it is withheld
///   precisely while that harvest is still owed, and not one poll longer. Once
///   the index is on the Pod the failure is reported immediately, even if the
///   sidecar has yet to drain: a wedged sidecar must not be able to hide a
///   verdict we already know.
///
/// EVERY other verdict passes through verbatim — an infra failure, a timeout, a
/// config failure or a lost Pod must never be masked as Running — and the
/// scheduler's next poll drives settle to done, so this is deterministic rather
/// than a sleep. Withholding is bounded by the engine's step-timeout backstop.
/// A **third** rule, added by ADR-0061 part 4 (s4). See
/// [`workspace_snapshot_lost`] — the short version is that the two rules above
/// withhold `Succeeded` while the sidecar *proxy* says the settle is in flight,
/// and this one refuses `Succeeded` outright when the sidecar is gone and the
/// settle's *product* is not there.
fn settled_state(pod: &Pod, harvesting: bool) -> ExecState {
    let state = pod_state(pod);
    let settling = init_container_running(pod, WORKSPACE_EGRESS_CONTAINER);
    match state {
        ExecState::Succeeded if settling => ExecState::Running,
        // ADR-0061 part 4: an Attempt is not `Succeeded` until its Workspace
        // Snapshot is durable. The sidecar being gone is not proof that it is —
        // see `workspace_snapshot_lost`.
        ExecState::Succeeded if workspace_snapshot_lost(pod) => ExecState::Failed {
            exit_code: None,
            // The step's process DID run, so a side effect is possible: the
            // engine's retry has to be the at-least-once kind (CONTEXT.md §2),
            // not the "safe, it never started" kind.
            class: FailureClass::Infra {
                never_started: false,
            },
            cause: Some(
                "workspace snapshot lost: the step exited 0 but its egress sidecar \
                 died before the drain recorded a root — the emptyDir is gone and \
                 the snapshot can never be produced (ADR-0061 part 4)"
                    .to_string(),
            ),
        },
        ExecState::Failed {
            class: FailureClass::Step,
            ..
        } if settling && artifact_harvest_owed(pod, harvesting) => ExecState::Running,
        other => other,
    }
}

/// Is this a Pod on the workspace flow?
///
/// Derived from the Pod's **spec** — the egress barrier container exists — rather
/// than from an annotation. Two reasons: it survives a control-plane restart with
/// no in-memory state, and it does not re-create the conflation git-bug 7f05f39
/// was about (an annotation whose *presence* meant one thing and whose
/// *emptiness* meant another).
fn is_workspace_pod(pod: &Pod) -> bool {
    pod.spec
        .as_ref()
        .and_then(|s| s.init_containers.as_ref())
        .into_iter()
        .flatten()
        .any(|c| c.name == WORKSPACE_EGRESS_CONTAINER)
}

/// Did this Pod's Workspace Snapshot get lost? (ADR-0061 part 4.)
///
/// True when a workspace Pod's step exited **0**, its egress sidecar is **gone**,
/// and `scarab.io/workspace-root` was never recorded. That combination means the
/// snapshot does not exist and never will: the workspace lived on the Pod's
/// `emptyDir`, the barrier that held the Pod open for the drain has been released
/// or killed, and there is nothing left to drain.
///
/// # Why this is not already covered by withholding
///
/// [`settled_state`]'s first rule withholds `Succeeded` while the sidecar is
/// running, on the reasoning that `drive_workspace` releases the sidecar only
/// *after* patching the root. That is true of the release, and it is not true of
/// every way a sidecar can die:
///
/// - the node vanishes (spot reclaim, the case ADR-0061 part 4 is written about) —
///   the kubelet stops reporting, and whatever status was last observed is what
///   `settled_state` sees;
/// - the Pod is deleted and the 600 s `terminationGracePeriodSeconds` elapses, so
///   the kubelet **SIGKILL**s a sidecar that was deliberately ignoring SIGTERM;
/// - anything else that kills the container out from under the barrier.
///
/// In each of those the sidecar has terminated without the annotation, so the
/// proxy says "settled" while the product is absent. Reporting `Succeeded` there
/// puts a claim in the durable record that the record cannot back — a step marked
/// green with no evidence, and dependents launched against a snapshot that does
/// not exist. That is the one thing this product may not do (CONTEXT.md §2), and
/// it is worth restating that the engine will not catch it downstream: on
/// `Succeeded` it records `output()` *if `Some`* and finalises the step either way
/// (`scheduler.rs`, the `ExecState::Succeeded` arm).
///
/// # Why a failure and not more withholding
///
/// Withholding forever would hang the Run, and there is nothing to wait for: the
/// drain leg is gated on the sidecar being alive, so a dead sidecar means the
/// snapshot can never appear. ADR-0061 part 4 names the answer — *"missing/late
/// durability report = infrastructure failure class → retry"*. So the verdict is
/// `Infra { never_started: false }`, the attempt's budget is consumed, and a
/// bounded retry re-runs the step on (probably) another node. A step that keeps
/// losing its node keeps failing and dead-letters, which is honest.
///
/// A **failed** step is deliberately not covered: its workspace is not an output
/// (`drive_workspace` snapshots successful steps only), so there is no snapshot to
/// owe. Its *artifacts* are owed, and [`artifact_harvest_owed`] is that rule.
fn workspace_snapshot_lost(pod: &Pod) -> bool {
    is_workspace_pod(pod)
        && step_terminated_exit(pod) == Some(0)
        && !init_container_running(pod, WORKSPACE_EGRESS_CONTAINER)
        && !pod
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|a| a.get(ANNOTATION_WS_ROOT).is_some_and(|v| !v.is_empty()))
}

/// Whether the settle leg still owes an artifact harvest for this Pod (ADR-0052)
/// — pure and derived entirely from the Pod, so it survives a control-plane
/// restart and every re-poll reaches the same verdict.
///
/// Owed for EVERY terminated step, whatever its exit code (a28a173): a failing
/// step's artifacts are evidence — often THE evidence — and the scheduler indexes
/// them off the `Failed` verdict exactly as it does off `Succeeded`.
///
/// The egress barrier must stay closed while this is true: releasing it lets the
/// Pod report its terminal phase, and the orchestrator indexes a step's artifacts
/// off that verdict exactly once (98ea804). It flips false only once the harvest
/// has recorded its index on the Pod — including the EMPTY index of a step that
/// published nothing — which is also what makes a re-harvest once-only.
fn artifact_harvest_owed(pod: &Pod, harvesting: bool) -> bool {
    harvesting
        && step_terminated_exit(pod).is_some()
        && !pod
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|a| a.contains_key(ANNOTATION_ARTIFACTS))
}

// `pack_dir` lived here: it tarred a control-plane tempdir for the feed leg's
// `exec` tunnel and for the debug Pod's copy of it. Both are deleted (ADR-0061
// s3-feed), and nothing else ever packed a tar — the drain and the artifact
// harvest only *unpack* one (`unpack_dir`), because the tar is produced inside
// the Pod by `tar -cf -`.

/// Shape of a materialized workspace tree: `(files, bytes, walk_ms)`.
///
/// ADR-0061 s0 measurement support. **File count is the number that matters**:
/// `S3Storage::materialize` and `ingest_dir` both walk the tree one file at a
/// time, awaiting a `get_blob` / `head`+`put` round-trip each, so the CAS legs
/// scale with file count while the `exec` tar legs scale with bytes. Reporting
/// both is what lets the two be told apart.
///
/// Best-effort and non-fatal: an unreadable entry is skipped rather than failing
/// a Step boundary for a measurement. Cheap — it stats a tree that was just
/// written, so it is warm in page cache — and timed separately so it can be
/// subtracted from the phase it sits next to.
fn dir_stats(dir: &std::path::Path) -> (u64, u64, u128) {
    let start = std::time::Instant::now();
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(entry.path()),
                Ok(_) => {
                    files += 1;
                    if let Ok(md) = entry.metadata() {
                        bytes += md.len();
                    }
                }
                Err(_) => continue,
            }
        }
    }
    (files, bytes, start.elapsed().as_millis())
}

/// Unpack a tar stream into `dir`.
fn unpack_dir(bytes: &[u8], dir: &std::path::Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(bytes);
    archive.unpack(dir).map_err(|e| format!("tar unpack: {e}"))
}

#[async_trait]
impl Executor for K8sExecutor {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        let pods = self.pods()?;
        let name = pod_name(step);

        // ADR-0061 s3-feed, fail-closed and BEFORE any Pod exists: a Step with
        // input snapshots is provisioned by the fetcher talking to the workspace
        // service, and there is no control-plane feed to fall back on. Refusing
        // here (rather than creating a Pod whose `/workspace` would silently be
        // empty) is the whole point — an empty workspace does not fail, it
        // produces a wrong answer, and a wrong answer in the durable record is
        // the one thing this product may not do (CONTEXT.md §2).
        workspace_feed_is_satisfiable(spec, self.workspace_cas.is_some(), self.workspace_fetch.as_ref())
            .map_err(ExecError::Launch)?;

        // Re-attach: if the Pod already exists (a prior launch we may not have
        // observed completing), adopt it instead of creating a duplicate.
        let existing = pods
            .get_opt(&name)
            .await
            .map_err(|e| ExecError::Launch(e.to_string()))?;
        if let Some(pod) = existing {
            // Refresh the short-TTL clone credential and re-mint the expiring
            // workspace token on re-drives (ADR-0045 / ADR-0061).
            self.ensure_step_secrets(&name, &pod, step, spec).await?;
            return Ok(ExecHandle(name));
        }

        let pod = build_pod(
            &name,
            &self.namespace,
            step,
            spec,
            self.results_egress.as_ref(),
            self.default_step_timeout_secs,
            self.workspace_cas.is_some(),
            &self.clone_image,
            self.workspace_fetch.as_ref(),
        );
        // ADR-0055: stamp the baseline, merge the named PlacementProfiles + the
        // governed k8s_overlay, and apply requested resources — fallible because a
        // named profile might not exist in the registry (fail-closed).
        let pod = apply_placement(pod, spec, &self.placement).map_err(ExecError::Launch)?;
        match pods.create(&PostParams::default(), &pod).await {
            Ok(created) => {
                self.ensure_step_secrets(&name, &created, step, spec).await?;
                Ok(ExecHandle(name))
            }
            // A concurrent launcher (or a re-drive) won the create race — the
            // Pod now exists. Adopt it, but still upsert its Secrets: the create
            // winner may have created the Pod and not (yet) provisioned them,
            // and a Pod that mounts a Secret which never appears stays in
            // FailedMount forever. Fetch the live Pod so the Secrets are
            // owner-referenced to it, mirroring the re-attach path above.
            Err(kube::Error::Api(ae)) if ae.code == 409 => {
                if let Some(pod) = pods
                    .get_opt(&name)
                    .await
                    .map_err(|e| ExecError::Launch(e.to_string()))?
                {
                    self.ensure_step_secrets(&name, &pod, step, spec).await?;
                }
                Ok(ExecHandle(name))
            }
            Err(e) => Err(ExecError::Launch(e.to_string())),
        }
    }

    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
        let pods = self.pods()?;
        match pods
            .get_opt(&handle.0)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?
        {
            Some(pod) => {
                // Workspace orchestration (ADR-0029/0045): feed the init
                // container / snapshot the finished workspace. Both legs are
                // idempotent and derive ALL state from the Pod itself, so an
                // adopted Pod after a control-plane restart resumes cleanly.
                if let Some(cas) = &self.workspace_cas {
                    match self.drive_workspace(&pods, &pod, cas).await {
                        Ok(()) => {}
                        // Permanent and author-fixable (ADR-0007): the step ran and
                        // exited 0, but did not honor its declared `outputs:`
                        // contract. Retrying the identical spec cannot fix it, so
                        // fail fast with a developer verdict (`Config`) rather than
                        // burn the infra retry budget and dead-letter as an
                        // operator problem it is not. The Pod is left for its logs.
                        Err(DriveErr::OutputContract(msg)) => {
                            eprintln!("scarab-executor: {msg} (pod {})", handle.0);
                            // Terminal verdict: nothing more is owed from the
                            // workspace, so release the egress sidecar before
                            // reporting it (e10cf7e). This bail happens BEFORE
                            // drive_workspace's own release, and the sidecar
                            // ignores SIGTERM by design — without the touch it
                            // loops forever and the Pod stays phase-Running,
                            // holding node resources and its emptyDir, matched
                            // by no reaper. Releasing forfeits nothing: the Pod
                            // terminates and is kept for its logs exactly like
                            // the normal failed path — deliberately NOT an
                            // executor.cancel from the scheduler, which would
                            // delete the Pod and destroy the logs. Best-effort,
                            // and best-effort is ALL there is: this return is a
                            // terminal verdict, the scheduler settles the
                            // Attempt and marks it dispatched, and NO later
                            // poll re-enters this arm — so this exec is the one
                            // and only release attempt. If it fails, the
                            // SIGTERM-proof sidecar strands the Pod
                            // phase-Running until an operator acts (the error
                            // log below names the remedy). The failure must
                            // never mask or alter the classification. Artifact
                            // harvest is skipped on this arm because drive
                            // bailed before it — pre-existing, out of scope
                            // here.
                            if let Err(e) = self.release_egress_sidecar(&pods, &handle.0).await {
                                tracing::error!(
                                    pod = %handle.0,
                                    container = WORKSPACE_EGRESS_CONTAINER,
                                    marker = %egress_done_marker(),
                                    error = %e,
                                    "egress release failed on a terminal verdict — this was \
                                     the ONLY attempt (the verdict settles and no poll \
                                     re-enters this arm), so the SIGTERM-proof sidecar now \
                                     strands the Pod phase-Running until an operator acts: \
                                     `kubectl exec` a `touch` of the marker in the egress \
                                     container, or delete the Pod (forfeiting its logs)"
                                );
                            }
                            return Ok(ExecState::Failed {
                                exit_code: None,
                                class: FailureClass::Config,
                                cause: Some(msg),
                            });
                        }
                        // The step's evidence could not be made durable and
                        // never will be / took too long (ADR-0064, 4cf03d7).
                        // Through the NORMAL Failed path — like OutputContract
                        // above, NOT `ExecError::Other` — so it presents as a
                        // prompt, legible infra failure carrying its cause,
                        // never as a step-budget timeout. Post-start: the step
                        // DID run, so the author-gated `retry:` budget is the
                        // at-least-once bound (ADR-0047).
                        Err(DriveErr::EvidenceLost { cause }) => {
                            eprintln!(
                                "scarab-executor: evidence lost for pod {}: {cause}",
                                handle.0
                            );
                            // Same terminal-verdict release as the
                            // OutputContract arm above (e10cf7e): the
                            // escalation clock has ended, nothing more is owed
                            // from the workspace, and leaving the SIGTERM-proof
                            // sidecar waiting strands the Pod phase-Running
                            // forever. Best-effort, and — as above — this is
                            // the ONE attempt: the verdict settles, no poll
                            // re-enters this arm, so a failed exec here leaves
                            // the Pod stranded until an operator touches the
                            // marker or deletes it (the error log names the
                            // remedy). The release never masks or alters the
                            // classification.
                            if let Err(e) = self.release_egress_sidecar(&pods, &handle.0).await {
                                tracing::error!(
                                    pod = %handle.0,
                                    container = WORKSPACE_EGRESS_CONTAINER,
                                    marker = %egress_done_marker(),
                                    error = %e,
                                    "egress release failed on a terminal verdict — this was \
                                     the ONLY attempt (the verdict settles and no poll \
                                     re-enters this arm), so the SIGTERM-proof sidecar now \
                                     strands the Pod phase-Running until an operator acts: \
                                     `kubectl exec` a `touch` of the marker in the egress \
                                     container, or delete the Pod (forfeiting its logs)"
                                );
                            }
                            return Ok(ExecState::Failed {
                                exit_code: None,
                                class: FailureClass::Infra {
                                    never_started: false,
                                },
                                cause: Some(cause),
                            });
                        }
                        Err(DriveErr::Transient(e)) => {
                            return Err(ExecError::Other(format!("workspace: {e}")));
                        }
                    }
                    // The settle (workspace snapshot + artifact harvest) runs in
                    // the egress NATIVE sidecar and patches the durable Pod
                    // annotations that output()/artifacts() read. A native
                    // sidecar does NOT gate the Pod phase, so a workspace Pod
                    // reports its terminal phase the instant the step exits —
                    // BEFORE the settle has patched them, which would let the
                    // scheduler read an empty artifact set exactly once and index
                    // nothing (98ea804 for Succeeded, a28a173 for the step's own
                    // Failed). drive_workspace touches egress-done only AFTER
                    // patching, so the sidecar terminating is the settle-complete
                    // signal: re-read and withhold those two verdicts while their
                    // settle products are outstanding (see `settled_state`).
                    // Every other verdict passes through verbatim so infra
                    // failures are never masked; the scheduler re-polls next tick
                    // and drives settle to done (deterministic, no sleeps).
                    let pod = match pods
                        .get_opt(&handle.0)
                        .await
                        .map_err(|e| ExecError::Other(e.to_string()))?
                    {
                        Some(pod) => pod,
                        None => return Ok(ExecState::Lost),
                    };
                    return Ok(settled_state(&pod, self.artifact_store.is_some()));
                }
                Ok(pod_state(&pod))
            }
            // The Pod is gone (evicted, GC'd, node lost) — the backend lost it.
            None => Ok(ExecState::Lost),
        }
    }

    /// The step's output workspace: the CAS root ingested at egress — pruned to
    /// the step's declared `outputs:` paths when it has any (ADR-0007) — recorded
    /// as a Pod annotation (durable with the Pod across restarts).
    async fn output(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        if self.workspace_cas.is_none() {
            return Ok(None);
        }
        let pods = self.pods()?;
        let pod = pods
            .get_opt(&handle.0)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?;
        Ok(pod.and_then(|p| {
            p.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(ANNOTATION_WS_ROOT))
                .filter(|v| !v.is_empty())
                .cloned()
        }))
    }

    /// The published snapshot's content identity (ADR-0061 s8), from the sibling
    /// Pod annotation the drain leg wrote in the same patch as the root.
    async fn output_identity(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        if self.workspace_cas.is_none() {
            return Ok(None);
        }
        let pods = self.pods()?;
        let pod = pods
            .get_opt(&handle.0)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?;
        Ok(pod.and_then(|p| {
            p.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(ANNOTATION_WS_IDENTITY))
                .filter(|v| !v.is_empty())
                .cloned()
        }))
    }

    /// The durability tier the published snapshot's flush earned (ADR-0064
    /// s2) — `"object"` | `"separate-volume"` | `"warm-only"` — from the
    /// sibling Pod annotation the drain leg wrote in the same patch as the
    /// root. Absent/empty annotation → `Ok(None)`: no Depot wired (approved
    /// deferral — the drain wrote the durable store directly), a pre-s2 Pod,
    /// or an old Depot that affirmed durability without naming a tier.
    async fn output_durability(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        if self.workspace_cas.is_none() {
            return Ok(None);
        }
        let pods = self.pods()?;
        let pod = pods
            .get_opt(&handle.0)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?;
        Ok(pod.and_then(|p| {
            p.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(ANNOTATION_WS_DURABILITY))
                .filter(|v| !v.is_empty())
                .cloned()
        }))
    }

    /// The artifacts the step published (ADR-0052), from the Pod annotation
    /// the harvest recorded (durable with the Pod across restarts).
    async fn artifacts(
        &self,
        handle: &ExecHandle,
    ) -> Result<Vec<scarab_engine::ArtifactMeta>, ExecError> {
        if self.artifact_store.is_none() {
            return Ok(Vec::new());
        }
        let pods = self.pods()?;
        let pod = pods
            .get_opt(&handle.0)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?;
        Ok(pod
            .and_then(|p| {
                p.metadata
                    .annotations
                    .as_ref()
                    .and_then(|a| a.get(ANNOTATION_ARTIFACTS))
                    .and_then(|v| serde_json::from_str(v).ok())
            })
            .unwrap_or_default())
    }

    async fn cancel(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        let pods = self.pods()?;
        // Graceful: SIGTERM, then SIGKILL after the grace period (ADR-0020).
        let params = DeleteParams {
            grace_period_seconds: Some(CANCEL_GRACE_SECS as u32),
            ..DeleteParams::default()
        };
        match pods.delete(&handle.0, &params).await {
            Ok(_) => Ok(()),
            // Already gone is success for cancel.
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
            Err(e) => Err(ExecError::Other(e.to_string())),
        }
    }

    // `results` intentionally uses the port default (empty): on k8s the egress
    // **sidecar** (see `build_pod`/`egress_sidecar`, ADR-0042) writes results
    // straight to Postgres via the fence-scoped results API, so the orchestrator
    // reads them from the store — not back through this method. (A
    // `restartPolicy: Never` Pod is gone after completion and the log stream is
    // best-effort, so post-hoc scraping is not an option; hence the sidecar.)
    // Remaining to make this live end-to-end: the sidecar image (the `scarab` CLI
    // that drains + POSTs on shutdown) and a real-cluster lifecycle test.

    /// Open a live tail of the step Pod's stdout/stderr via the k8s log endpoint
    /// (ADR-0013): `follow: true` on the deterministic `{run, step, attempt}` Pod,
    /// pinned to the `step` container so the egress sidecar's output never mixes
    /// in. The control plane drains the returned [`LogChunks`] into the log
    /// pipeline. Best-effort: if the Pod's log is not yet available (still Pending,
    /// container not started) this errors and the caller retries on a later tick.
    async fn log_stream(&self, step: &StepRun) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        let pods = self.pods()?;
        let name = pod_name(step);
        let params = LogParams {
            follow: true,
            container: Some(STEP_CONTAINER.to_string()),
            ..LogParams::default()
        };
        let reader = pods
            .log_stream(&name, &params)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?;
        Ok(Some(Box::new(PodLogChunks {
            reader: Box::pin(reader),
        })))
    }

    /// Open a live tail of ONE of the step Pod's sidecar containers (ADR-0058
    /// evidence) — identical to [`log_stream`](Self::log_stream) but pinned to
    /// `container` (a `service-{i}` sidecar) instead of the main `step` container,
    /// so a step's sidecar output is captured as its own stream. Same
    /// deterministic `{run, step, attempt}` Pod (`pod_name`); best-effort like the
    /// step tail (a Pod/container not yet started errors and is retried later).
    async fn sidecar_log_stream(
        &self,
        step: &StepRun,
        container: &str,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        let pods = self.pods()?;
        let name = pod_name(step);
        let params = LogParams {
            follow: true,
            container: Some(container.to_string()),
            ..LogParams::default()
        };
        let reader = pods
            .log_stream(&name, &params)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?;
        Ok(Some(Box::new(PodLogChunks {
            reader: Box::pin(reader),
        })))
    }

    /// Provision a shared service (ADR-0058): create the service Pod, the cluster
    /// DNS Service, and the opt-in-scoped NetworkPolicy. Idempotent — a 409 on any
    /// object means a prior launch created it, so we adopt rather than fail. The
    /// handle is the service Pod name (what readiness/teardown address).
    ///
    /// NOTE: this executor uses ONE fixed namespace for every Take (there is no
    /// namespace-per-Take substrate here). Per-Take isolation is therefore carried
    /// by the resource NAME: `service_resource_name` folds the `take`, so a
    /// Rerun's fresh instance is a distinct Pod/NetworkPolicy that never collides
    /// with the prior Take's still-terminating one. The durable per-Take instancing
    /// is mirrored at the engine layer (`RunService` keyed `{run, take}`).
    async fn launch_service(
        &self,
        run: &RunId,
        take: i64,
        name: &str,
        spec: &scarab_pipeline::ServiceSpec,
    ) -> Result<ExecHandle, ExecError> {
        let client = self.client.clone().ok_or(ExecError::Unavailable)?;
        let pods: Api<Pod> = Api::namespaced(client.clone(), &self.namespace);
        let services: Api<Service> = Api::namespaced(client.clone(), &self.namespace);
        let netpols: Api<NetworkPolicy> = Api::namespaced(client, &self.namespace);

        // Take-scope the Pod / NetworkPolicy so a Rerun's fresh Take never collides
        // with the prior (still-terminating) Take in this single-namespace executor
        // (see `service_resource_name`). The k8s Service keeps the stable declared
        // DNS name (`<name>:<port>`) — it is deliberately NOT Take-scoped, and its
        // `{run, name}` selector converges on whichever Take's Pod is currently
        // live as the stale one drains.
        let pod = build_service_pod(&run.0, name, take, &self.namespace, spec);
        let handle = pod.metadata.name.clone().unwrap_or_default();
        // Ignore 409 (adopt existing) on every object — launch is idempotent.
        adopt_conflict(pods.create(&PostParams::default(), &pod).await)?;
        adopt_conflict(
            services
                .create(
                    &PostParams::default(),
                    &build_service(&run.0, name, &self.namespace, &spec.ports),
                )
                .await,
        )?;
        adopt_conflict(
            netpols
                .create(
                    &PostParams::default(),
                    &build_network_policy(&run.0, name, take, &self.namespace, &spec.ports),
                )
                .await,
        )?;
        Ok(ExecHandle(handle))
    }

    /// A shared service is ready when its Pod reports the `Ready` condition
    /// `True` (its readiness probe has passed) — the readiness-gate signal.
    async fn service_ready(&self, handle: &ExecHandle) -> Result<bool, ExecError> {
        let pods = self.pods()?;
        let Some(pod) = pods
            .get_opt(&handle.0)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?
        else {
            return Ok(false);
        };
        let ready = pod
            .status
            .and_then(|s| s.conditions)
            .map(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
            .unwrap_or(false);
        Ok(ready)
    }

    /// Tear down a shared service's Pod, Service, and NetworkPolicy (ADR-0058).
    /// Idempotent: a 404 (already gone) is success. The Service and NetworkPolicy
    /// share the Pod's resource name except the Service, which is named for the
    /// declared service — recovered from the Pod's `scarab.io/service` label.
    async fn teardown_service(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        let client = self.client.clone().ok_or(ExecError::Unavailable)?;
        let pods: Api<Pod> = Api::namespaced(client.clone(), &self.namespace);
        let netpols: Api<NetworkPolicy> = Api::namespaced(client.clone(), &self.namespace);
        let services: Api<Service> = Api::namespaced(client, &self.namespace);
        let dp = DeleteParams::default();
        // Recover the declared service name (the Service object's name) from the
        // Pod label before deleting the Pod.
        let svc_name = pods
            .get_opt(&handle.0)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?
            .and_then(|p| p.metadata.labels)
            .and_then(|l| l.get("scarab.io/service").cloned());
        ignore_missing(pods.delete(&handle.0, &dp).await)?;
        ignore_missing(netpols.delete(&handle.0, &dp).await)?;
        if let Some(sn) = svc_name {
            ignore_missing(services.delete(&sn, &dp).await)?;
        }
        Ok(())
    }

    /// Best-effort live tail of a shared service's Pod logs (ADR-0058 evidence):
    /// the service Pod's `service` container, `follow: true` — the same k8s log
    /// machinery step logs use ([`log_stream`](Self::log_stream)), addressed by
    /// the launch `handle` (the service Pod name). Errors (Pod still Pending / no
    /// log yet) are the caller's to retry; the tail is never load-bearing.
    async fn service_log_stream(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        let pods = self.pods()?;
        let params = LogParams {
            follow: true,
            container: Some(SERVICE_POD_CONTAINER.to_string()),
            ..LogParams::default()
        };
        let reader = pods
            .log_stream(&handle.0, &params)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?;
        Ok(Some(Box::new(PodLogChunks {
            reader: Box::pin(reader),
        })))
    }
}

/// A [`LogChunks`] over a k8s Pod's followed log stream. Wraps kube's
/// `AsyncBufRead` and reads it in [`LOG_CHUNK_BYTES`]-sized bites; an empty read
/// is end-of-stream (the followed Pod finished and the API closed the log).
struct PodLogChunks {
    reader: std::pin::Pin<Box<dyn AsyncBufRead + Send>>,
}

#[async_trait]
impl LogChunks for PodLogChunks {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ExecError> {
        let mut buf = vec![0u8; LOG_CHUNK_BYTES];
        let n = self
            .reader
            .read(&mut buf)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?;
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n);
        Ok(Some(buf))
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-testable without a cluster)
// ---------------------------------------------------------------------------

/// Deterministic Pod name for a step's fence `{run, step, attempt}`. Because it
/// is a pure function of the fence, the same step always maps to the same Pod
/// name across process restarts — which is what makes `launch` re-attach rather
/// than relaunch. A short content hash keeps distinct fences from colliding
/// after DNS-1123 sanitisation, and the whole thing stays within 63 chars.
// ── Debug shell: interactive attach (the debug surface) ────────────────────

/// A live interactive attach to a running step's container: an interactive
/// `sh` with a TTY, for the operator to bridge to a client terminal. Only a
/// *running* step has a live Pod to attach to — step Pods are
/// `restartPolicy: Never` and gone once the step ends.
pub struct AttachIo {
    /// Combined program output (a TTY merges stdout + stderr).
    pub output: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    /// The shell's stdin — client keystrokes go here.
    pub input: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
    /// Keeps the underlying attached process (and its connection task) alive as
    /// long as the caller holds the IO. Opaque so callers need no `kube` types.
    pub _process: Box<dyn std::any::Any + Send>,
}

/// Opens an interactive shell into a *running* step's container — the debug
/// surface. Backends that can't exec return [`ExecError::Unavailable`].
#[async_trait]
pub trait StepAttacher: Send + Sync {
    async fn attach(&self, step: &StepRun) -> Result<AttachIo, ExecError>;
}

impl K8sExecutor {
    /// Open an interactive `sh` (TTY) in `pod`'s step container — the shared
    /// primitive behind both live-attach and debug-pod. A TTY multiplexes
    /// stderr into stdout, so stderr must be off.
    async fn attach_pod(&self, pod: &str) -> Result<AttachIo, ExecError> {
        let client = self.client.clone().ok_or(ExecError::Unavailable)?;
        let pods: Api<Pod> = Api::namespaced(client, &self.namespace);
        let params = AttachParams::default()
            .container(STEP_CONTAINER)
            .stdin(true)
            .stdout(true)
            .stderr(false)
            .tty(true);
        let mut proc = pods
            .exec(pod, ["sh"], &params)
            .await
            .map_err(|e| ExecError::Other(format!("attach to {pod}: {e}")))?;
        let output = proc
            .stdout()
            .ok_or_else(|| ExecError::Other("attach: no stdout stream".into()))?;
        let input = proc
            .stdin()
            .ok_or_else(|| ExecError::Other("attach: no stdin stream".into()))?;
        Ok(AttachIo {
            output: Box::pin(output),
            input: Box::pin(input),
            _process: Box::new(proc),
        })
    }

    /// Poll until `container` (init or main) reaches a running state, or time out.
    async fn wait_container(
        &self,
        pods: &Api<Pod>,
        pod: &str,
        container: &str,
        is_init: bool,
        timeout_secs: u64,
    ) -> Result<(), ExecError> {
        for _ in 0..(timeout_secs * 5) {
            if let Ok(p) = pods.get(pod).await {
                let running = if is_init {
                    init_container_running(&p, container)
                } else {
                    p.status
                        .as_ref()
                        .and_then(|s| s.container_statuses.as_ref())
                        .map(|cs| {
                            cs.iter().any(|c| {
                                c.name == container
                                    && c.state
                                        .as_ref()
                                        .and_then(|st| st.running.as_ref())
                                        .is_some()
                            })
                        })
                        .unwrap_or(false)
                };
                if running {
                    return Ok(());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        Err(ExecError::Other(format!(
            "timed out waiting for container {container} in {pod}"
        )))
    }

    fn debug_pod_name(step: &StepRun) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let slug = truncate(&sanitize_dns(&step.step.0), 32);
        let hash = fnv1a(&format!("{}/{}/{nanos}", step.run.0, step.step.0));
        format!("scarab-debug-{slug}-{hash:08x}")
    }
}

#[async_trait]
impl StepAttacher for K8sExecutor {
    async fn attach(&self, step: &StepRun) -> Result<AttachIo, ExecError> {
        self.attach_pod(&pod_name(step)).await
    }
}

/// A created debug pod — an ephemeral shell reproduction of a finished step.
pub struct DebugPod {
    pub name: String,
}

/// Reproduces a *finished* step in a fresh ephemeral Pod — its image, its output
/// workspace snapshot re-materialized at `/workspace` — running `sleep` so an
/// operator can shell in. The debug Pod carries no hardened baseline (it runs as
/// the image's own user, so any image can boot) and is not governed; it's a
/// throwaway. Backends that can't reproduce return [`ExecError::Unavailable`].
#[async_trait]
pub trait DebugLauncher: Send + Sync {
    async fn launch_debug(
        &self,
        step: &StepRun,
        image: &str,
        snapshot_root: Option<&str>,
        ttl_secs: u64,
    ) -> Result<DebugPod, ExecError>;
    async fn attach_debug(&self, pod: &str) -> Result<AttachIo, ExecError>;
    async fn teardown_debug(&self, pod: &str) -> Result<(), ExecError>;
}

#[async_trait]
impl DebugLauncher for K8sExecutor {
    async fn launch_debug(
        &self,
        step: &StepRun,
        image: &str,
        snapshot_root: Option<&str>,
        ttl_secs: u64,
    ) -> Result<DebugPod, ExecError> {
        let client = self.client.clone().ok_or(ExecError::Unavailable)?;
        let pods: Api<Pod> = Api::namespaced(client, &self.namespace);
        let name = Self::debug_pod_name(step);

        let ws_mount = VolumeMount {
            name: WORKSPACE_VOLUME.to_string(),
            mount_path: WORKSPACE_MOUNT_PATH.to_string(),
            ..Default::default()
        };
        // The debug Pod's feed goes through the SAME fetcher as a real Step's
        // (ADR-0061 s3-feed). It used to carry a copy-pasted control-plane feed —
        // its own `cas.materialize` into a tempdir, its own `pack_dir`, its own
        // `tar -xf -` over exec, its own `chmod -R g+rwX` — which is git-bug
        // 64897db: two implementations of one contract, only one of them tested,
        // free to drift on merge order and on the ownership fix-up. There is now
        // one.
        let mut init_containers: Vec<Container> = Vec::new();
        let mut volumes: Vec<Volume> = vec![Volume {
            name: WORKSPACE_VOLUME.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        }];
        if let Some(root) = snapshot_root {
            // No workspace service ⇒ no way to re-materialise the snapshot, and
            // no fallback (see `workspace_feed_is_satisfiable`). Reporting
            // `Unavailable` is what the old code did when the CAS was unwired; the
            // reason has moved, the verdict has not.
            let fetch = self
                .workspace_fetch
                .as_ref()
                .ok_or(ExecError::Unavailable)?;
            let roots = vec![root.to_string()];
            let (container, token_volume) =
                workspace_fetch_container(&name, fetch, &roots, &ws_mount);
            init_containers.push(container);
            volumes.push(token_volume);
        }
        let shell = Container {
            name: STEP_CONTAINER.to_string(),
            image: Some(image.to_string()),
            // Keep the Pod alive so it can be shelled into; TTL-bounded.
            command: Some(vec!["sleep".into(), ttl_secs.to_string()]),
            working_dir: Some(WORKSPACE_MOUNT_PATH.to_string()),
            volume_mounts: Some(vec![ws_mount]),
            ..Default::default()
        };
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(std::collections::BTreeMap::from([
                    (
                        "app.kubernetes.io/managed-by".to_string(),
                        "scarab".to_string(),
                    ),
                    ("scarab.io/debug".to_string(), "true".to_string()),
                    ("scarab.io/run".to_string(), sanitize_label(&step.run.0)),
                    ("scarab.io/step".to_string(), sanitize_label(&step.step.0)),
                ])),
                ..Default::default()
            },
            spec: Some(PodSpec {
                restart_policy: Some("Never".to_string()),
                init_containers: (!init_containers.is_empty()).then_some(init_containers),
                containers: vec![shell],
                // Same workspace-ownership contract as a real step Pod: the
                // re-materialized snapshot is owned by `WORKSPACE_GID` and only
                // group-writable, so the debug shell must be in that group or the
                // workspace it was opened to poke at is read-only (b04697f).
                security_context: Some(k8s_openapi::api::core::v1::PodSecurityContext {
                    fs_group: Some(WORKSPACE_GID),
                    supplemental_groups: Some(vec![WORKSPACE_GID]),
                    ..Default::default()
                }),
                volumes: Some(volumes),
                ..Default::default()
            }),
            ..Default::default()
        };
        pods.create(&PostParams::default(), &pod)
            .await
            .map_err(|e| ExecError::Other(format!("create debug pod: {e}")))?;

        // The token Secret must exist before the kubelet mounts it, but the Pod
        // must exist before the Secret can be owner-referenced to it — the same
        // create-then-provision order every other per-Pod credential uses, and the
        // reason a FailedMount here resolves itself on the kubelet's retry rather
        // than wedging.
        if let (Some(root), Some(fetch)) = (snapshot_root, self.workspace_fetch.as_ref()) {
            let created = pods
                .get_opt(&name)
                .await
                .map_err(|e| ExecError::Other(e.to_string()))?;
            self.put_workspace_token_secret(
                &name,
                created.as_ref(),
                self.mint_debug_workspace_token(fetch, step, root, ttl_secs),
            )
            .await?;
        }

        // Wait for the shell container to be running before we let a client
        // attach. The fetcher is an ordinary init container, so the kubelet will
        // not start this until the workspace is provisioned — the wait that used
        // to be a marker file is now just the container ordering.
        self.wait_container(&pods, &name, STEP_CONTAINER, false, 90)
            .await?;
        Ok(DebugPod { name })
    }

    async fn attach_debug(&self, pod: &str) -> Result<AttachIo, ExecError> {
        self.attach_pod(pod).await
    }

    async fn teardown_debug(&self, pod: &str) -> Result<(), ExecError> {
        let client = self.client.clone().ok_or(ExecError::Unavailable)?;
        let pods: Api<Pod> = Api::namespaced(client, &self.namespace);
        pods.delete(pod, &DeleteParams::default().grace_period(0))
            .await
            .map_err(|e| ExecError::Other(format!("delete debug pod {pod}: {e}")))?;
        Ok(())
    }
}

pub fn pod_name(step: &StepRun) -> String {
    let attempt = step
        .current_attempt()
        .map(|a| a.id.0.as_str())
        .unwrap_or("0");
    let fence = format!("{}/{}/{}", step.run.0, step.step.0, attempt);
    let hash = fnv1a(&fence);
    let slug = truncate(&sanitize_dns(&format!("{}-{}", step.step.0, attempt)), 40);
    // `scarab-` prefix + slug + 8-hex hash; always a valid DNS-1123 label.
    format!("scarab-{slug}-{hash:08x}")
}

/// The default canonical scarab-clone image (ADR-0045). Overridable via
/// `SCARAB_CLONE_IMAGE` (digest-pin in production).
pub const DEFAULT_CLONE_IMAGE: &str = "ghcr.io/thulasi-ram/scarab-clone:edge";
/// The tmpfs mount the clone credential is delivered on (ADR-0045 §Token
/// delivery): a per-Pod k8s Secret volume (tmpfs on the node), read by
/// GIT_ASKPASS — the token is never in env, URL, or argv.
const CLONE_SECRETS_MOUNT_PATH: &str = "/scarab/secrets";
const CLONE_SECRETS_VOLUME: &str = "scarab-clone-token";
/// The key/file name the token lives under.
const CLONE_TOKEN_KEY: &str = "clone-token";

/// The in-Pod workspace root (ADR-0007/0008): steps run here; the clone step
/// and every producer write here; the snapshot covers it (incl. `.git`).
pub const WORKSPACE_MOUNT_PATH: &str = "/workspace";
/// The gid that **owns** the workspace: the control plane materializes the CAS
/// tree as its own (65532) uid/gid, so every restored `/workspace` lands owned by
/// this group and is made group-writable at feed time (see `drive_workspace`).
///
/// Membership in this group is therefore the ONLY thing that lets a step write
/// its workspace under the ADR-0039 restricted baseline (all capabilities —
/// including `DAC_OVERRIDE` — are dropped, so even a `run_as_root` uid-0 step is
/// subject to ordinary DAC). Every workspace Pod puts it in `supplementalGroups`
/// so that guarantee is explicit rather than a side effect of `fsGroup`.
const WORKSPACE_GID: i64 = 65532;
/// The uid Scarab's own helper containers run as (ADR-0039: non-root, always).
/// Numerically equal to [`WORKSPACE_GID`] — `docker/wsfetch` and `docker/clone`
/// both create user 65532 with a matching primary group — but they are different
/// concepts, so they are different constants: this one is *who the fetcher is*,
/// that one is *who owns the workspace*.
const WORKSPACE_HELPER_UID: i64 = 65532;
/// The workspace `emptyDir` volume name.
const WORKSPACE_VOLUME: &str = "scarab-workspace";
/// Control-plane→Pod handshake dir (markers only; NEVER part of the snapshot).
const CTL_MOUNT_PATH: &str = "/scarab-ctl";
const CTL_VOLUME: &str = "scarab-ctl";
/// The marker the egress sidecar's wait-loop blocks on, and the one command
/// that sets it. A function (not inlined format strings) so the two spellings
/// — the sidecar's `until [ -f … ]` and the release's `touch` — provably name
/// the same file: the exec itself needs a cluster, but the agreement of the
/// paths is assertable in-process (see the
/// `egress_release_touches_the_marker_the_sidecar_waits_on` test).
fn egress_done_marker() -> String {
    format!("{CTL_MOUNT_PATH}/egress-done")
}
fn egress_release_cmd() -> String {
    format!("touch {}", egress_done_marker())
}
/// The helper image for the workspace **egress** container: it only runs
/// `sh`/`tar`/`sleep`, so a pinned busybox suffices. (`:1.36` and not `:latest`
/// deliberately — `busybox:latest` defaults to root, which the ADR-0039
/// restricted baseline refuses with `CreateContainerConfigError`.)
///
/// The *init* side stopped being a busybox doorstop in ADR-0061 s3-feed; it is
/// [`DEFAULT_WSFETCH_IMAGE`] now. This constant survives only for the drain
/// barrier, and dies with s3-drain (git-bug `7f05f39`).
const WORKSPACE_HELPER_IMAGE: &str = "busybox:1.36";
/// Names of the workspace helper containers.
const WORKSPACE_INIT_CONTAINER: &str = "scarab-workspace-init";
const WORKSPACE_EGRESS_CONTAINER: &str = "scarab-workspace-egress";

/// The default workspace-fetcher image (ADR-0061 s3-feed): the Scarab-owned
/// init container that materialises a Step's input snapshots from the workspace
/// service. Overridable via `SCARAB_WSFETCH_IMAGE`; digest-pin in production, as
/// with the clone image.
///
/// ⚠ **ADR-0061 s3-feed: DELETE ME with the driver (git-bug 0628369).**
pub const DEFAULT_WSFETCH_IMAGE: &str = "ghcr.io/thulasi-ram/scarab-wsfetch:edge";

/// The volume name carrying the per-Pod workspace-token Secret. Distinct from
/// `CLONE_SECRETS_VOLUME` even though both land on
/// [`workspace_token::WORKSPACE_SECRETS_MOUNT_PATH`]: they are mounted into
/// *different containers* (the token into the fetcher, the clone credential into
/// the step), so neither credential's presence implies the other's.
const WORKSPACE_TOKEN_VOLUME: &str = "scarab-workspace-token";

/// The env var carrying the input **Workspace Snapshot** roots into the fetcher,
/// in merge order. Roots, not workspaces (CONTEXT.md §4.2): the fetcher
/// materialises the mutable Workspace *from* these immutable trees. Must agree
/// with `scarab-wsfetch`'s `ROOTS_ENV`.
const WSFETCH_ROOTS_ENV: &str = "SCARAB_SNAPSHOT_ROOTS";
/// The env var telling the fetcher where to build the Workspace.
const WSFETCH_TARGET_ENV: &str = "SCARAB_WORKSPACE_TARGET";

/// How many Step Pods this process has provisioned with the **eager** fetcher.
///
/// Guard #1 of ADR-0061 D2.3's three anti-calcification guards, control-plane
/// half. The fetcher itself prints `mode=eager (…)` into every Step Pod's log,
/// but a one-shot process cannot hold a counter, so the accumulating one lives
/// here — where a `/metrics` scrape or a test can read it and see the stepping
/// stone still being stood on.
static EAGER_FETCH_TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Step Pods provisioned by the eager fetcher since process start (ADR-0061
/// s3-feed). This number should go to zero and stay there once the node driver
/// lands; while it climbs, the stepping stone is load-bearing.
pub fn eager_fetch_total() -> u64 {
    EAGER_FETCH_TOTAL.load(std::sync::atomic::Ordering::Relaxed)
}

/// The per-Pod Secret carrying the workspace token (owner-referenced to the Pod,
/// so it is garbage-collected with it).
fn workspace_secret_name(pod_name: &str) -> String {
    format!("{pod_name}-workspace")
}

/// Can this Step's workspace actually be provisioned? (ADR-0061 s3-feed.)
///
/// `Err` when the Step declares input snapshots, the workspace flow is on, and no
/// workspace service is configured. **Fail-closed, and there is no fallback by
/// design**: the control-plane `kubectl exec` tar feed is deleted, not kept for a
/// rainy day (ADR-0061 D2.3 — an eager path is permitted as a temporally ordered
/// replacement, never as a runtime branch). The alternative to failing is a Pod
/// whose `/workspace` is silently empty, which does not fail: it produces a
/// *wrong answer*, and the Attempt would then claim to have tested a tree that was
/// never there.
///
/// Pure, so the rule is testable without a cluster.
pub fn workspace_feed_is_satisfiable(
    spec: &StepSpec,
    workspace: bool,
    fetch: Option<&WorkspaceFetch>,
) -> Result<(), String> {
    if workspace && !spec.workspace_inputs.is_empty() && fetch.is_none() {
        return Err(format!(
            "this step inherits {} workspace snapshot(s) but no workspace service is \
             configured (ADR-0061): set SCARAB_WORKSPACE_TOKEN_SECRET and \
             SCARAB_WORKSPACE_URL. Refusing to launch a step whose /workspace would be \
             silently empty.",
            spec.workspace_inputs.len()
        ));
    }
    Ok(())
}

/// The fetcher init container plus the tmpfs Secret volume it reads its token
/// from (ADR-0061 s3-feed).
///
/// Shared by [`build_pod`] and the debug Pod, which is the point: the debug Pod
/// used to carry a **copy-pasted** feed implementation (git-bug `64897db`), so
/// the two could drift on the merge order, the ownership fix-up or the fidelity
/// rules — and only one of them had tests.
///
/// Security posture: the ADR-0039 restricted baseline, pinned to uid 65532. The
/// fetcher holds a credential the untrusted step container never sees, so it gets
/// the same treatment as any other trusted helper: non-root, all capabilities
/// dropped, no privilege escalation, `RuntimeDefault` seccomp.
fn workspace_fetch_container(
    pod_name: &str,
    fetch: &WorkspaceFetch,
    roots: &[String],
    ws_mount: &VolumeMount,
) -> (Container, Volume) {
    EAGER_FETCH_TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The other half of guard #1: loud on the control plane too, so an operator
    // reading server logs sees the stepping stone without reading Pod logs.
    tracing::warn!(
        pod = %pod_name,
        inputs = roots.len(),
        image = %fetch.fetcher_image,
        "mode=eager (ADR-0061 s3-feed stepping stone — the node driver replaces this)"
    );
    let container = Container {
        name: WORKSPACE_INIT_CONTAINER.to_string(),
        image: Some(fetch.fetcher_image.clone()),
        // The image's entrypoint IS the fetcher; nothing to override.
        env: Some(vec![
            env_var(workspace_token::WORKSPACE_URL_ENV, &fetch.url),
            env_var(
                workspace_token::WORKSPACE_TOKEN_FILE_ENV,
                &workspace_token::workspace_token_path(),
            ),
            // Merge order is the LIST order (ADR-0007): later inputs overlay
            // earlier ones, so this join must not be sorted.
            env_var(WSFETCH_ROOTS_ENV, &roots.join(",")),
            env_var(WSFETCH_TARGET_ENV, WORKSPACE_MOUNT_PATH),
        ]),
        volume_mounts: Some(vec![
            ws_mount.clone(),
            VolumeMount {
                name: WORKSPACE_TOKEN_VOLUME.to_string(),
                mount_path: workspace_token::WORKSPACE_SECRETS_MOUNT_PATH.to_string(),
                read_only: Some(true),
                ..Default::default()
            },
        ]),
        security_context: Some(SecurityContext {
            run_as_non_root: Some(true),
            run_as_user: Some(WORKSPACE_HELPER_UID),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".to_string()]),
                add: None,
            }),
            allow_privilege_escalation: Some(false),
            privileged: Some(false),
            seccomp_profile: Some(SeccompProfile {
                type_: "RuntimeDefault".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let volume = Volume {
        name: WORKSPACE_TOKEN_VOLUME.to_string(),
        secret: Some(k8s_openapi::api::core::v1::SecretVolumeSource {
            secret_name: Some(workspace_secret_name(pod_name)),
            ..Default::default()
        }),
        ..Default::default()
    };
    (container, volume)
}
/// Pod annotations: the input CAS roots (set at build, read at feed time so a
/// resumed control plane needs no in-memory state) and the ingested output
/// root (patched after egress; `Executor::output` reads it — durable with the
/// Pod across control-plane restarts).
const ANNOTATION_WS_INPUTS: &str = "scarab.io/workspace-inputs";
/// The Pod annotation carrying the harvested artifacts' metadata (ADR-0052) —
/// JSON `[{name,size,content_type,object_key}]`, durable with the Pod.
const ANNOTATION_ARTIFACTS: &str = "scarab.io/artifacts";
/// The Pod annotation carrying the step's artifact publication globs.
const ANNOTATION_ARTIFACT_GLOBS: &str = "scarab.io/artifact-globs";
/// Where a step publishes artifacts of record (ADR-0008/0052 convention).
pub const ARTIFACTS_MOUNT_PATH: &str = "/scarab/artifacts";
const ARTIFACTS_VOLUME: &str = "scarab-artifacts";
const ANNOTATION_WS_ROOT: &str = "scarab.io/workspace-root";
/// The Pod annotation carrying the published snapshot's **content identity**
/// (ADR-0061 s8): the merkle fold with mtimes dropped — *what* the workspace
/// holds, as against `ANNOTATION_WS_ROOT`'s *where the bytes are*. This is what
/// restart invalidation compares, and the two are separate because a tree hash
/// moves with every file's mtime, so a re-run can never reproduce its own root
/// (git-bug `945b1f4`). Empty when the store computed none.
const ANNOTATION_WS_IDENTITY: &str = "scarab.io/snapshot-identity";
/// The Pod annotation recording the durability tier the published snapshot's
/// flush EARNED (ADR-0064 s2): `"object"` | `"separate-volume"` |
/// `"warm-only"`, straight off the Depot's flush verdict. Written in the SAME
/// patch as [`ANNOTATION_WS_ROOT`]/[`ANNOTATION_WS_IDENTITY`] (crash-atomic
/// with the root claim); `Executor::output_durability` reads it. Deliberately
/// ABSENT — never a guessed value — when there is nothing truthful to stamp:
/// no Depot wired (the drain wrote the durable store directly; approved
/// deferral), or a `Durable`-without-tier reply from an old Depot (skew
/// window during a rolling upgrade).
const ANNOTATION_WS_DURABILITY: &str = "scarab.io/workspace-durability";
/// The Pod annotation carrying the step's authored `outputs:` paths (ADR-0007),
/// comma-separated. Absent/empty = publish the whole workspace. Read at egress,
/// so an adopted Pod prunes identically with no in-memory state.
const ANNOTATION_WS_OUTPUTS: &str = "scarab.io/workspace-outputs";
/// The Pod annotation recording (epoch ms) the FIRST time this Pod's workspace
/// drain failed against the Depot (ADR-0064 control-plane half, ticket
/// 4cf03d7). Written once, only if absent — it anchors the wall-clock bound in
/// [`drain_failure_verdict`], so a Depot outage escalates to
/// [`DriveErr::EvidenceLost`] after [`WS_DRAIN_ESCALATION_MS`] instead of
/// grinding until the step-budget timeout. Durable with the Pod, so a
/// control-plane restart mid-outage neither resets nor loses the clock. Only
/// drain-leg failures (ingest / prune / flush against the Depot) touch it;
/// artifact-harvest failures keep their own transient loop.
const ANNOTATION_WS_DRAIN_FIRST_FAILURE: &str = "scarab.dev/ws-drain-first-failure-ms";
/// How long the drain may keep failing transiently before the Attempt is
/// failed as [`DriveErr::EvidenceLost`]: 5 minutes. Chosen to be longer than a
/// Depot helm rollout (a routine deploy must never fail Attempts) and far
/// shorter than a step budget (the default is an hour — the architect's ruling
/// on 4cf03d7 is that a Depot outage fails Attempts PROMPTLY and LEGIBLY,
/// never disguised as a step timeout).
const WS_DRAIN_ESCALATION_MS: i64 = 5 * 60 * 1000;
/// Grace period for workspace Pods: the egress sidecar ignores SIGTERM and
/// waits for the control plane to snapshot `/workspace`, bounded by this.
const WORKSPACE_TERMINATION_GRACE_SECS: i64 = 600;

/// The global default step deadline in seconds (ADR-0047): mandatory, so a
/// hung Pod can never wedge a run forever. Overridable per step (`timeout:`)
/// and globally (`SCARAB_STEP_TIMEOUT_SECS`).
pub const DEFAULT_STEP_TIMEOUT_SECS: u32 = 3_600;

/// Absolute in-Pod path of the shared results directory (ADR-0008/0042). The
/// step writes `<name>.json` files here; the egress sidecar reads them.
const RESULTS_MOUNT_PATH: &str = "/scarab/results";
/// Name of the shared `emptyDir` between the step and the egress sidecar.
const RESULTS_VOLUME: &str = "scarab-results";

/// Build the bare Pod for a step: one container running `spec`, restartPolicy
/// Never, with the fence injected as env vars for cooperating idempotency
/// (ADR-0021) and labels for observability. When `egress` is set, also mounts a
/// shared results volume and adds the trusted results-egress **sidecar**
/// (ADR-0042) — a native sidecar (an `initContainer` with `restartPolicy: Always`)
/// that drains `/scarab/results` to the fence-scoped results API after the step
/// exits. The untrusted step container never holds the token; only the sidecar does.
///
/// `fetch` is the ADR-0061 workspace service. When the workspace flow is on and
/// the Step has input snapshots, it becomes the `scarab-workspace-init` fetcher
/// container. **Precondition:** if `workspace` is true and
/// `spec.workspace_inputs` is non-empty, `fetch` must be `Some` — see
/// [`workspace_feed_is_satisfiable`], which `launch` calls before it ever gets
/// here. Passing `None` anyway builds a Pod with no feed at all, which is why
/// the check is at the launch site and not buried in this assembly.
#[allow(clippy::too_many_arguments)] // the one Pod-assembly point; splitting hides the shape
pub fn build_pod(
    name: &str,
    namespace: &str,
    step: &StepRun,
    spec: &StepSpec,
    egress: Option<&ResultsEgress>,
    default_timeout_secs: u32,
    workspace: bool,
    clone_image: &str,
    fetch: Option<&WorkspaceFetch>,
) -> Pod {
    debug_assert!(
        !(workspace && !spec.workspace_inputs.is_empty() && fetch.is_none()),
        "ADR-0061: a Step with input snapshots needs a workspace service; \
         workspace_feed_is_satisfiable() must gate this"
    );
    let attempt = step
        .current_attempt()
        .map(|a| a.id.0.clone())
        .unwrap_or_else(|| "0".to_string());

    let mut env: Vec<EnvVar> = spec
        .env
        .iter()
        .map(|(k, v)| EnvVar {
            name: k.clone(),
            value: Some(v.clone()),
            value_from: None,
        })
        .collect();
    // Fence env vars: hand each Attempt its monotonic {run, step, attempt} token.
    for (k, v) in [
        ("SCARAB_RUN", step.run.0.clone()),
        ("SCARAB_STEP", step.step.0.clone()),
        ("SCARAB_ATTEMPT", attempt.clone()),
    ] {
        env.push(EnvVar {
            name: k.to_string(),
            value: Some(v),
            value_from: None,
        });
    }
    // With egress, tell the step where to write results (ADR-0008 convention).
    if egress.is_some() {
        env.push(EnvVar {
            name: "SCARAB_RESULTS".to_string(),
            value: Some(RESULTS_MOUNT_PATH.to_string()),
            value_from: None,
        });
    }

    let results_mount = egress.map(|_| VolumeMount {
        name: RESULTS_VOLUME.to_string(),
        mount_path: RESULTS_MOUNT_PATH.to_string(),
        ..Default::default()
    });

    // Workspace machinery (ADR-0029/0045): the shared /workspace emptyDir the
    // step runs in, plus the control handshake dir (markers only — never part
    // of the snapshot).
    let workspace_mount = workspace.then(|| VolumeMount {
        name: WORKSPACE_VOLUME.to_string(),
        mount_path: WORKSPACE_MOUNT_PATH.to_string(),
        ..Default::default()
    });
    let ctl_mount = workspace.then(|| VolumeMount {
        name: CTL_VOLUME.to_string(),
        mount_path: CTL_MOUNT_PATH.to_string(),
        ..Default::default()
    });

    // Artifacts of record (ADR-0052): an emptyDir the step publishes into,
    // harvested post-step by the control plane via the workspace egress leg
    // (so it exists only when the workspace flow is on).
    let artifacts_mount = workspace.then(|| VolumeMount {
        name: ARTIFACTS_VOLUME.to_string(),
        mount_path: ARTIFACTS_MOUNT_PATH.to_string(),
        ..Default::default()
    });
    if workspace {
        env.push(EnvVar {
            name: "SCARAB_ARTIFACTS".to_string(),
            value: Some(ARTIFACTS_MOUNT_PATH.to_string()),
            value_from: None,
        });
        // The clone step provisions `/workspace` as its own (non-root) uid, but
        // downstream steps run as whatever uid their image/grant dictates — often
        // a different one. Since git 2.35.2 (CVE-2022-24765) git refuses a repo
        // whose worktree is owned by another uid ("detected dubious ownership"),
        // which would make every git command in a consuming step fail unless the
        // author remembered a `safe.directory` incantation. The workspace is
        // executor-provisioned and the Pod is single-tenant and ephemeral, so the
        // shared-machine threat that check guards against doesn't apply here —
        // mark it trusted for the whole container via git's env-based config
        // (no gitconfig file, no HOME dependency). `*` covers nested paths like
        // submodules, which each get their own ownership check.
        for (k, v) in [
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "safe.directory"),
            ("GIT_CONFIG_VALUE_0", "*"),
        ] {
            env.push(EnvVar {
                name: k.to_string(),
                value: Some(v.to_string()),
                value_from: None,
            });
        }
    }

    let mut step_mounts: Vec<VolumeMount> = Vec::new();
    if let Some(m) = results_mount.clone() {
        step_mounts.push(m);
    }
    if let Some(m) = workspace_mount.clone() {
        step_mounts.push(m);
    }
    if let Some(m) = artifacts_mount.clone() {
        step_mounts.push(m);
    }

    // Clone steps (ADR-0045): the canonical scarab-clone image (never the
    // author's), context via env — and the credential ONLY via the tmpfs
    // secret volume (never env/URL/argv). `clone_image` is threaded through
    // `build_pod`'s caller (the executor's config).
    let mut clone_token_volume: Option<Volume> = None;
    let (image, command) = if let Some(clone) = &spec.clone {
        for (k, v) in [
            ("SCARAB_CLONE_URL", clone.url.clone()),
            ("SCARAB_CLONE_SHA", clone.sha.clone()),
            (
                "SCARAB_CLONE_DEPTH",
                if clone.depth_full {
                    "full".into()
                } else {
                    "1".into()
                },
            ),
            ("SCARAB_CLONE_SUBMODULES", clone.submodules.to_string()),
            ("SCARAB_CLONE_LFS", clone.lfs.to_string()),
        ] {
            env.push(EnvVar {
                name: k.to_string(),
                value: Some(v),
                value_from: None,
            });
        }
        if let Some(cred) = &clone.credential {
            // Point the askpass helper at the tmpfs file; the username is not
            // secret. The token itself rides ONLY in the mounted Secret.
            env.push(EnvVar {
                name: "SCARAB_CLONE_TOKEN_FILE".to_string(),
                value: Some(format!("{CLONE_SECRETS_MOUNT_PATH}/{CLONE_TOKEN_KEY}")),
                value_from: None,
            });
            env.push(EnvVar {
                name: "SCARAB_CLONE_USERNAME".to_string(),
                value: Some(cred.username.clone()),
                value_from: None,
            });
            step_mounts.push(VolumeMount {
                name: CLONE_SECRETS_VOLUME.to_string(),
                mount_path: CLONE_SECRETS_MOUNT_PATH.to_string(),
                read_only: Some(true),
                ..Default::default()
            });
            clone_token_volume = Some(Volume {
                name: CLONE_SECRETS_VOLUME.to_string(),
                secret: Some(k8s_openapi::api::core::v1::SecretVolumeSource {
                    secret_name: Some(clone_secret_name(name)),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        (clone_image.to_string(), Vec::new())
    } else {
        (spec.image.clone(), spec.command.clone())
    };

    // Build steps (ADR-0018): rootless BuildKit (never the author's image),
    // registry auth ONLY via a mounted dockerconfigjson (a per-Pod Secret —
    // tmpfs), and the image digest emitted as the `image` step result when
    // the egress sidecar is present.
    let mut registry_volume: Option<Volume> = None;
    let (image, command, args) = if let Some(build) = &spec.build {
        env.push(EnvVar {
            // Rootless buildkitd cannot use the process sandbox.
            name: "BUILDKITD_FLAGS".into(),
            value: Some("--oci-worker-no-process-sandbox".into()),
            value_from: None,
        });
        let has_auth = build.registry_auth_json.is_some() || build.derived_auth.is_some();
        if has_auth {
            env.push(EnvVar {
                name: "DOCKER_CONFIG".into(),
                value: Some(REGISTRY_AUTH_MOUNT_PATH.to_string()),
                value_from: None,
            });
            step_mounts.push(VolumeMount {
                name: REGISTRY_AUTH_VOLUME.to_string(),
                mount_path: REGISTRY_AUTH_MOUNT_PATH.to_string(),
                read_only: Some(true),
                ..Default::default()
            });
            registry_volume = Some(Volume {
                name: REGISTRY_AUTH_VOLUME.to_string(),
                secret: Some(k8s_openapi::api::core::v1::SecretVolumeSource {
                    secret_name: Some(registry_secret_name(name)),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        (
            BUILDKIT_IMAGE.to_string(),
            vec!["buildctl-daemonless.sh".to_string()],
            buildctl_args(build, egress.is_some()),
        )
    } else {
        (image, command, Vec::new())
    };

    // Per-attempt OIDC token (ADR-0015): tmpfs Secret mount + a pointer env
    // var. The token itself NEVER rides in env or argv.
    let mut oidc_volume: Option<Volume> = None;
    if spec.oidc_token.is_some() {
        env.push(EnvVar {
            name: "SCARAB_OIDC_TOKEN_FILE".to_string(),
            value: Some(format!("{OIDC_TOKEN_MOUNT_PATH}/{OIDC_TOKEN_KEY}")),
            value_from: None,
        });
        step_mounts.push(VolumeMount {
            name: OIDC_TOKEN_VOLUME.to_string(),
            mount_path: OIDC_TOKEN_MOUNT_PATH.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
        oidc_volume = Some(Volume {
            name: OIDC_TOKEN_VOLUME.to_string(),
            secret: Some(k8s_openapi::api::core::v1::SecretVolumeSource {
                secret_name: Some(oidc_secret_name(name)),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    let container = Container {
        name: STEP_CONTAINER.to_string(),
        image: Some(image),
        command: (!command.is_empty()).then_some(command),
        args: (!args.is_empty()).then_some(args),
        env: Some(env),
        security_context: Some(if spec.build.is_some() {
            build_security_context()
        } else {
            step_security_context(spec)
        }),
        volume_mounts: (!step_mounts.is_empty()).then_some(step_mounts),
        // Steps run in the workspace (ADR-0008 convention).
        working_dir: workspace.then(|| WORKSPACE_MOUNT_PATH.to_string()),
        ..Default::default()
    };

    let mut labels = std::collections::BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "scarab".to_string(),
        ),
        ("scarab.io/run".to_string(), sanitize_label(&step.run.0)),
        ("scarab.io/step".to_string(), sanitize_label(&step.step.0)),
        ("scarab.io/attempt".to_string(), sanitize_label(&attempt)),
    ]);
    // Shared-service opt-in (ADR-0058): one label per `uses:` name so each named
    // service's NetworkPolicy admits this Pod (least-privilege — a Pod that opts
    // into nothing carries no service label and every service NetworkPolicy
    // denies it). The label key is the service-name-scoped selector the matching
    // `build_network_policy` ingress rule looks for.
    for name in &spec.uses {
        labels.insert(service_uses_label(name), "true".to_string());
    }

    // Egress wiring (ADR-0042): a shared emptyDir + the sidecar as a native
    // sidecar (initContainer with restartPolicy Always), so it starts alongside
    // the step and the kubelet terminates it after the step exits — its window to
    // drain results — without blocking the Pod's terminal phase.
    let mut volumes: Vec<Volume> = Vec::new();
    let mut init_containers: Vec<Container> = Vec::new();
    if let Some(e) = egress {
        volumes.push(Volume {
            name: RESULTS_VOLUME.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        });
        init_containers.push(egress_sidecar(
            step,
            &attempt,
            results_mount.as_ref().unwrap(),
            e,
        ));
    }
    if let Some(v) = clone_token_volume {
        volumes.push(v);
    }
    if let Some(v) = registry_volume {
        volumes.push(v);
    }
    if let Some(v) = oidc_volume {
        volumes.push(v);
    }
    let mut annotations = std::collections::BTreeMap::new();
    if spec.build.is_some() {
        // AppArmor unconfined for rootless buildkit (user-namespace worker).
        annotations.insert(
            format!("container.apparmor.security.beta.kubernetes.io/{STEP_CONTAINER}"),
            "unconfined".to_string(),
        );
    }
    if workspace {
        volumes.push(Volume {
            name: WORKSPACE_VOLUME.to_string(),
            // ADR-0061 s3-feed: DELETE ME with the driver (git-bug 0628369).
            // ↑ THE one line that chooses the volume kind, and the one line s2b
            // replaces with a `CSIVolumeSource` for `workspace.scarab.io`. An
            // `emptyDir` means the whole snapshot is copied onto this node before
            // the Step starts (eager); the driver mounts it as a read-only lower
            // layer with a pod-local writable upper layer and transfers only what
            // the Step actually reads (lazy). Everything else in this slice is
            // preparation for changing this expression.
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        });
        volumes.push(Volume {
            name: CTL_VOLUME.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        });
        // The input snapshot roots ride on the Pod as EVIDENCE — what this Pod was
        // fed, readable by anyone with `get pod`, durable across a control-plane
        // restart. Written only when there ARE inputs (git-bug 7f05f39): an
        // always-present-but-sometimes-empty annotation made "is this a workspace
        // Pod" and "does it need a feed" the same key with two meanings.
        // Deliberately NOT control flow any more — the fetcher gets its roots from
        // its own container env, which is the more durable of the two.
        if !spec.workspace_inputs.is_empty() {
            annotations.insert(
                ANNOTATION_WS_INPUTS.to_string(),
                spec.workspace_inputs.join(","),
            );
        }
        // Authored per-path publishing (ADR-0007) rides the Pod too, for the same
        // reason: the egress prune must be identical after a control-plane restart.
        if !spec.workspace_outputs.is_empty() {
            annotations.insert(
                ANNOTATION_WS_OUTPUTS.to_string(),
                spec.workspace_outputs.join(","),
            );
        }
        let ws = workspace_mount.clone().unwrap();
        let ctl = ctl_mount.clone().unwrap();
        // The feed (ADR-0061 s3-feed): an init container that FETCHES the merged
        // input snapshots from the workspace service and exits. There is no marker
        // file and no `until [ -f … ]` wait loop, because there is nothing left to
        // wait for — the container does the work itself. A Step with no inputs gets
        // no init container at all, rather than a `busybox` that runs `exit 0`.
        if !spec.workspace_inputs.is_empty() {
            if let Some(fetch) = fetch {
                let (container, token_volume) =
                    workspace_fetch_container(name, fetch, &spec.workspace_inputs, &ws);
                init_containers.push(container);
                volumes.push(token_volume);
            }
        }
        // Native egress sidecar: outlives the step (ignoring SIGTERM) until
        // the control plane has snapshotted /workspace into the CAS and
        // touches the release marker.
        init_containers.push(Container {
            name: WORKSPACE_EGRESS_CONTAINER.to_string(),
            image: Some(WORKSPACE_HELPER_IMAGE.to_string()),
            restart_policy: Some("Always".to_string()),
            command: Some(vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "trap '' TERM; until [ -f {} ]; do sleep 0.2; done",
                    egress_done_marker()
                ),
            ]),
            volume_mounts: Some(match artifacts_mount.clone() {
                Some(am) => vec![ws, ctl, am],
                None => vec![ws, ctl],
            }),
            ..Default::default()
        });
    }
    if workspace {
        volumes.push(Volume {
            name: ARTIFACTS_VOLUME.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        });
        if !spec.artifacts.is_empty() {
            annotations.insert(
                ANNOTATION_ARTIFACT_GLOBS.to_string(),
                serde_json::to_string(&spec.artifacts).unwrap_or_default(),
            );
        }
    }
    // Sidecar services (ADR-0058): each declared service co-locates in THIS Pod
    // as a native sidecar (an initContainer with `restartPolicy: Always`, reusing
    // the ADR-0042 machinery), reachable by the step at `localhost:<port>`. Its
    // optional readiness probe becomes the sidecar's **startupProbe**, so the
    // kubelet holds the MAIN step container until the service is ready — the
    // durable replacement for Woodpecker's `sleep 30s`. The service image is
    // author-supplied and runs under the ADR-0039 restricted baseline, **non-root
    // by default** (its own `run_as_user`/`run_as_root` govern it — independent of
    // the step's); governed caps/privileged keyed on a *service* digest are a
    // later slice — fail-closed.
    for (i, svc) in spec.services.iter().enumerate() {
        init_containers.push(service_sidecar(i, svc));
    }

    let volumes = (!volumes.is_empty()).then_some(volumes);
    let init_containers = (!init_containers.is_empty()).then_some(init_containers);

    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            annotations: (!annotations.is_empty()).then_some(annotations),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![container],
            init_containers,
            volumes,
            restart_policy: Some("Never".to_string()),
            // Step deadline (ADR-0047): enforced by the kubelet, so a hung Pod
            // dies even if the control plane is down. Exceeding it surfaces as
            // `DeadlineExceeded` → the `Timeout` failure class.
            active_deadline_seconds: Some(
                spec.timeout_seconds.unwrap_or(default_timeout_secs) as i64
            ),
            // Pod-level fsGroup + supplementalGroups.
            //
            // `fsGroup` is what the kubelet chowns the Pod's volumes to at mount
            // time. Workspace Pods want that to be the workspace gid; a non-root
            // sidecar service that pins a uid (ADR-0058) needs it to be *its* gid
            // so it can write its own data emptyDir. fsGroup is Pod-level, so only
            // one can win and the first service that pins a uid takes it.
            //
            // `supplementalGroups` is what decides whether the STEP can write the
            // CAS-restored `/workspace`, which is owned by `WORKSPACE_GID` and made
            // group-writable at feed time. Under the ADR-0039 baseline all caps
            // (incl. `DAC_OVERRIDE`) are dropped, so a step — even an admitted
            // `run_as_root` one at uid 0 — only gets there via group membership.
            // Grant it explicitly on every workspace Pod instead of relying on
            // fsGroup: otherwise a step that merely declares a uid-pinning sidecar
            // service loses the workspace group and fails with `Permission denied`
            // (git-bug b04697f). Costs no capability and changes no ownership.
            security_context: {
                let fs_group = spec
                    .services
                    .iter()
                    .find_map(service_fs_group)
                    .or_else(|| workspace.then_some(WORKSPACE_GID));
                let supplemental_groups = workspace.then(|| vec![WORKSPACE_GID]);
                (fs_group.is_some() || supplemental_groups.is_some()).then(|| {
                    k8s_openapi::api::core::v1::PodSecurityContext {
                        fs_group,
                        supplemental_groups,
                        ..Default::default()
                    }
                })
            },
            termination_grace_period_seconds: workspace.then_some(WORKSPACE_TERMINATION_GRACE_SECS),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Apply the operator placement config + the step's requested placement to a
/// freshly built Pod (ADR-0055). Overlay order (later wins): **baseline** → each
/// named **PlacementProfile** (in listed order) → the governed **`k8s_overlay`**.
/// Container resources are applied to the step container from the step's
/// `resources`, falling back per-field to the baseline default. Overlays are
/// RFC-7386 merge-patches (objects merge recursively; arrays/scalars replace).
///
/// Returns `Err` if a step names a PlacementProfile absent from the registry —
/// fail-closed, never a silently mis-scheduled Pod. A no-op (returns the Pod
/// unchanged) when there is no baseline, no profile, no overlay and no resources,
/// preserving the pre-0055 behavior exactly.
fn apply_placement(
    mut pod: Pod,
    spec: &StepSpec,
    placement: &PlacementConfig,
) -> Result<Pod, String> {
    // 1. Container resources (typed) onto the step container, baseline-defaulted.
    let cpu = spec
        .resources
        .cpu_millis
        .or(placement.default_resources.cpu_millis);
    let mem = spec
        .resources
        .memory_mib
        .or(placement.default_resources.memory_mib);
    if cpu.is_some() || mem.is_some() {
        if let Some(pod_spec) = pod.spec.as_mut() {
            if let Some(c) = pod_spec
                .containers
                .iter_mut()
                .find(|c| c.name == STEP_CONTAINER)
            {
                c.resources = Some(resource_requirements(cpu, mem));
            }
        }
    }

    // 2. Build the raw overlay: baseline → named profiles (in order) → k8s_overlay.
    let mut overlay = serde_json::Value::Null;
    if let Some(base) = &placement.baseline {
        json_merge(&mut overlay, base);
    }
    for name in &spec.placement_profiles {
        let profile = placement
            .profile(name)
            .ok_or_else(|| format!("unknown placement_profile `{name}` (not in the registry)"))?;
        if let Some(k8s) = &profile.k8s {
            json_merge(&mut overlay, k8s);
        }
    }
    if let Some(o) = &spec.k8s_overlay {
        json_merge(&mut overlay, o);
    }

    // 3. Merge the overlay onto the Pod (no-op when empty). k8s_openapi serializes
    // with the k8s JSON field names (nodeSelector, tolerations…), which is exactly
    // what an operator writes into a baseline/profile/overlay.
    if !overlay.is_null() {
        let mut pod_value = serde_json::to_value(&pod).map_err(|e| format!("pod encode: {e}"))?;
        json_merge(&mut pod_value, &overlay);
        pod = serde_json::from_value(pod_value)
            .map_err(|e| format!("pod decode after overlay merge: {e}"))?;
    }
    Ok(pod)
}

/// RFC-7386 JSON merge-patch: objects merge recursively; a `null` in the patch
/// deletes the key; every other value (scalar or array) replaces.
fn json_merge(base: &mut serde_json::Value, patch: &serde_json::Value) {
    use serde_json::Value;
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                if v.is_null() {
                    b.remove(k);
                } else {
                    json_merge(b.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

/// k8s `ResourceRequirements` with requests == limits (Guaranteed QoS) from
/// optional millicpu / MiB. Absent axes are simply not set.
fn resource_requirements(cpu_millis: Option<u32>, memory_mib: Option<u32>) -> ResourceRequirements {
    let mut map = std::collections::BTreeMap::new();
    if let Some(c) = cpu_millis {
        map.insert("cpu".to_string(), Quantity(format!("{c}m")));
    }
    if let Some(m) = memory_mib {
        map.insert("memory".to_string(), Quantity(format!("{m}Mi")));
    }
    ResourceRequirements {
        requests: Some(map.clone()),
        limits: Some(map),
        ..Default::default()
    }
}

/// The trusted results-egress sidecar (ADR-0042). A native sidecar sharing the
/// results volume, carrying the fence-scoped token and the results API URL, that
/// drains `/scarab/results` with a confirmed write after the step exits. The
/// token is `HMAC-SHA256(secret, "{run}:{step}:{attempt}")` — minted here (via
/// `sign_hex`, the same helper the server verifies with) and held only by this
/// container, never the untrusted step.
fn egress_sidecar(
    step: &StepRun,
    attempt: &str,
    results_mount: &VolumeMount,
    egress: &ResultsEgress,
) -> Container {
    let message = format!("{}:{}:{}", step.run.0, step.step.0, attempt);
    let token = scarab_forge_github::sign_hex(&egress.token_secret, message.as_bytes());
    let url = format!(
        "{}/v1/runs/{}/steps/{}/results",
        egress.base_url.trim_end_matches('/'),
        step.run.0,
        step.step.0
    );
    let env = vec![
        env_var("SCARAB_RESULTS", RESULTS_MOUNT_PATH),
        env_var("SCARAB_RESULTS_URL", &url),
        env_var("SCARAB_RESULTS_TOKEN", &token),
        env_var("SCARAB_ATTEMPT", attempt),
    ];
    Container {
        name: "scarab-results-egress".to_string(),
        image: Some(egress.sidecar_image.clone()),
        // Native sidecar: an initContainer that keeps running alongside the step.
        restart_policy: Some("Always".to_string()),
        env: Some(env),
        // Read-only view of the step's results.
        volume_mounts: Some(vec![VolumeMount {
            read_only: Some(true),
            ..results_mount.clone()
        }]),
        // The sidecar image's entrypoint (the `scarab` CLI) performs the drain +
        // confirmed POST on shutdown; the command is left to the image.
        ..Default::default()
    }
}

fn env_var(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        value_from: None,
    }
}

/// The container `SecurityContext` for a step (ADR-0039): a hardened, Kubernetes
/// "restricted"-equivalent **baseline** — `runAsNonRoot`, drop **ALL**
/// capabilities, `RuntimeDefault` seccomp, `allowPrivilegeEscalation: false` — on
/// top of which only the **admitted** grants on `spec` are applied. Anything not
/// admitted stays at the baseline (fail-closed); the executor never escalates
/// beyond what admission blessed.
fn step_security_context(spec: &StepSpec) -> SecurityContext {
    SecurityContext {
        // Baseline non-root; only an admitted `run_as_root` opts out (and then we
        // pin uid 0 explicitly).
        run_as_non_root: Some(!spec.run_as_root),
        run_as_user: spec.run_as_root.then_some(0),
        // Baseline drops everything; admitted capabilities are added back.
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: (!spec.add_capabilities.is_empty()).then(|| spec.add_capabilities.clone()),
        }),
        // Privilege escalation stays off unless the container is privileged.
        allow_privilege_escalation: Some(spec.privileged),
        privileged: Some(spec.privileged),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Name prefix for a sidecar service container (ADR-0058); the index makes each
/// service in a Step deterministic and unique (`service-0`, `service-1`, …).
const SERVICE_CONTAINER_PREFIX: &str = "service-";

/// Build the native-sidecar [`Container`] for a declared service (ADR-0058): an
/// `initContainer` with `restartPolicy: Always` co-located in the step's Pod
/// (localhost-reachable), its readiness probe wired as the **startupProbe** so
/// the main step container is held until the service is ready.
fn service_sidecar(index: usize, svc: &scarab_pipeline::ServiceSpec) -> Container {
    Container {
        name: format!("{SERVICE_CONTAINER_PREFIX}{index}"),
        image: Some(svc.image.clone()),
        command: (!svc.command.is_empty()).then(|| svc.command.clone()),
        args: (!svc.args.is_empty()).then(|| svc.args.clone()),
        env: (!svc.env.is_empty()).then(|| svc.env.iter().map(|(k, v)| env_var(k, v)).collect()),
        ports: (!svc.ports.is_empty()).then(|| {
            svc.ports
                .iter()
                .map(|p| ContainerPort {
                    container_port: *p as i32,
                    ..Default::default()
                })
                .collect()
        }),
        // Native sidecar: starts alongside the step and is terminated by the
        // kubelet after the main container exits — its lifecycle is the Step's
        // (fenced by inheritance, ADR-0058), dying with the Pod on every Attempt.
        restart_policy: Some("Always".to_string()),
        startup_probe: service_startup_probe(svc),
        security_context: Some(service_security_context(svc.run_as_user, svc.run_as_root)),
        ..Default::default()
    }
}

/// The `startupProbe` for a service sidecar (ADR-0058), derived from its `ready:`
/// probe: `tcp` (the default — a TCP-connect on the first declared port when
/// `ready:` is omitted), `exec`, or `http`. Returns `None` when there is nothing
/// to gate on (no probe and no declared port), so the main container starts
/// immediately alongside the sidecar.
fn service_startup_probe(svc: &scarab_pipeline::ServiceSpec) -> Option<Probe> {
    let (tcp, exec, http) = match &svc.ready {
        Some(ready) if !ready.exec.is_empty() => (None, Some(ready.exec.clone()), None),
        Some(ready) if ready.http.is_some() => {
            let h = ready.http.as_ref().unwrap();
            (None, None, Some((h.port, h.path.clone())))
        }
        Some(ready) if ready.tcp.is_some() => (ready.tcp, None, None),
        // No `ready:` (or an empty one): default to a TCP-connect on the first
        // declared port.
        _ => (svc.ports.first().copied(), None, None),
    };
    if tcp.is_none() && exec.is_none() && http.is_none() {
        return None;
    }
    Some(Probe {
        tcp_socket: tcp.map(|p| TCPSocketAction {
            port: IntOrString::Int(p as i32),
            host: None,
        }),
        exec: exec.map(|command| ExecAction {
            command: Some(command),
        }),
        http_get: http.map(|(p, path)| HTTPGetAction {
            port: IntOrString::Int(p as i32),
            path: Some(path),
            ..Default::default()
        }),
        // Poll briskly but allow a generous startup window (the durable
        // "wait until healthy", not a fixed sleep): ~2 minutes before failing.
        period_seconds: Some(2),
        failure_threshold: Some(60),
        timeout_seconds: Some(2),
        ..Default::default()
    })
}

/// The `SecurityContext` for a sidecar/standalone service container (ADR-0058):
/// the same hardened ADR-0039 "restricted" baseline as a step container —
/// `runAsNonRoot`, drop **ALL** capabilities, `RuntimeDefault` seccomp, no
/// privilege escalation. Non-root **by default**: when `run_as_user` pins the
/// image's built-in service uid, it becomes the container `runAsUser`/`runAsGroup`
/// (the Pod-level `fsGroup` is set separately by the builders so the data volume
/// is writable). Only the service's own **self-service** `run-as-root` grant opts
/// out (root inside the caps-dropped, unprivileged, seccomp-confined sandbox does
/// not escape it); governed `add-capabilities`/`privileged`, which are keyed on
/// the *service* image digest, are not applied here (fail-closed — a later slice).
fn service_security_context(run_as_user: Option<u32>, run_as_root: bool) -> SecurityContext {
    SecurityContext {
        run_as_non_root: Some(!run_as_root),
        // Root escape hatch pins uid 0; otherwise pin the author-supplied non-root
        // service uid/gid when given (else the image's own built-in user must be
        // non-root to satisfy `runAsNonRoot`).
        run_as_user: if run_as_root {
            Some(0)
        } else {
            run_as_user.map(|u| u as i64)
        },
        run_as_group: (!run_as_root)
            .then_some(run_as_user)
            .flatten()
            .map(|u| u as i64),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: None,
        }),
        allow_privilege_escalation: Some(false),
        privileged: Some(false),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The Pod-level `fsGroup` a non-root service needs so its `emptyDir` data volume
/// is group-writable — the standard k8s non-root pattern that lets e.g. the
/// official `postgres` image write `PGDATA` (ADR-0058 governance). `Some` only
/// when the service pins a non-root uid and is not the root escape hatch.
fn service_fs_group(svc: &scarab_pipeline::ServiceSpec) -> Option<i64> {
    (!svc.run_as_root)
        .then_some(svc.run_as_user)
        .flatten()
        .map(|u| u as i64)
}

// ---------------------------------------------------------------------------
// Shared services (ADR-0058): a Run-scoped standalone Pod + k8s Service (cluster
// DNS `<name>:<port>`) + a NetworkPolicy scoping reachability to opt-in Pods.
// These builders are pure (no client), so they are unit-tested against the typed
// specs with no live cluster; the `Executor` impl creates/deletes them.
// ---------------------------------------------------------------------------

/// Treat a create that 409s (already exists) as success — launch is idempotent
/// on the service fence, so a re-drive adopts the existing object.
fn adopt_conflict<T>(r: Result<T, kube::Error>) -> Result<(), ExecError> {
    match r {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(()),
        Err(e) => Err(ExecError::Launch(e.to_string())),
    }
}

/// Treat a delete that 404s (already gone) as success — teardown is idempotent.
fn ignore_missing<T>(r: Result<T, kube::Error>) -> Result<(), ExecError> {
    match r {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
        Err(e) => Err(ExecError::Other(e.to_string())),
    }
}

/// The k8s object name for a shared service's Pod / NetworkPolicy in a run's
/// namespace: `scarab-svc-<name>-<runhash>-t<take>`. Keyed on `{run, name, take}`
/// — the ADR-0058 instance key. This executor pins ONE fixed namespace for every
/// Take (not a namespace-per-Take substrate), so a human Rerun advances the Take
/// while the prior Take's Pod is still terminating in the SAME namespace. A
/// Take-agnostic name would collide: `pods.create` for the fresh Take would 409
/// onto the terminating prior Pod, `adopt_conflict` would swallow it, no fresh
/// Pod would be born, and readiness would never resolve → the service fails-closed
/// on a Rerun even though it ran fine earlier. Folding the Take in makes each
/// generation's resources distinct so the races never touch.
///
/// The `take` is folded into the hash (so distinctness holds for ANY i64) AND a
/// human-readable `-t<take>` suffix is appended; the whole thing is truncated to
/// the 63-char DNS-1123 label limit (the hash guarantees uniqueness even if the
/// suffix is clipped for an absurdly large take). DNS-1123-safe, ≤63 chars.
fn service_resource_name(run: &str, name: &str, take: i64) -> String {
    let hash = fnv1a(&format!("{run}\u{0}{take}"));
    let slug = truncate(&sanitize_dns(name), 30);
    truncate(&format!("scarab-svc-{slug}-{hash:08x}-t{take}"), 63)
}

/// The opt-in label key a Step's Pod carries for shared service `name` (ADR-0058)
/// — the selector a service's NetworkPolicy ingress rule matches. `scarab.io/`
/// prefixed and name-scoped so distinct services get distinct holes.
fn service_uses_label(name: &str) -> String {
    format!("scarab.io/uses.{}", sanitize_label(name))
}

/// Labels stamped on a shared service's Pod/Service/NetworkPolicy — the anchor
/// the Service selector, the NetworkPolicy podSelector, and teardown-by-label all
/// key on. `scarab.io/service` names the instance within the run.
fn service_labels(run: &str, name: &str) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "scarab".to_string(),
        ),
        ("scarab.io/run".to_string(), sanitize_label(run)),
        ("scarab.io/service".to_string(), sanitize_label(name)),
    ])
}

/// Build the standalone **service Pod** for a shared service (ADR-0058): the
/// service image runs as the Pod's single main container (not a sidecar), under
/// the ADR-0039 restricted baseline (reusing the slice-1 security context). Its
/// `ready:` probe becomes the container's **readinessProbe** so the k8s Service
/// only routes once it is ready — the same builder slice-1 uses for a sidecar's
/// startupProbe. `restartPolicy: Always` keeps a flaky service alive; the run's
/// teardown removes it.
pub fn build_service_pod(
    run: &str,
    name: &str,
    take: i64,
    namespace: &str,
    svc: &scarab_pipeline::ServiceSpec,
) -> Pod {
    let container = Container {
        name: SERVICE_POD_CONTAINER.to_string(),
        image: Some(svc.image.clone()),
        command: (!svc.command.is_empty()).then(|| svc.command.clone()),
        args: (!svc.args.is_empty()).then(|| svc.args.clone()),
        env: (!svc.env.is_empty()).then(|| svc.env.iter().map(|(k, v)| env_var(k, v)).collect()),
        ports: (!svc.ports.is_empty()).then(|| {
            svc.ports
                .iter()
                .map(|p| ContainerPort {
                    container_port: *p as i32,
                    ..Default::default()
                })
                .collect()
        }),
        readiness_probe: service_startup_probe(svc),
        security_context: Some(service_security_context(svc.run_as_user, svc.run_as_root)),
        ..Default::default()
    };
    Pod {
        metadata: ObjectMeta {
            name: Some(service_resource_name(run, name, take)),
            namespace: Some(namespace.to_string()),
            labels: Some(service_labels(run, name)),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![container],
            // A standalone backing service is long-lived within the run; keep it
            // up on crash (teardown, not exit, ends it).
            restart_policy: Some("Always".to_string()),
            // Non-root default (ADR-0058): a pinned uid gets a matching Pod-level
            // fsGroup so the service's `emptyDir` data volume is group-writable
            // (the standard k8s pattern that lets the stock postgres image write
            // PGDATA). None when root or no uid pinned.
            security_context: service_fs_group(svc).map(|g| {
                k8s_openapi::api::core::v1::PodSecurityContext {
                    fs_group: Some(g),
                    ..Default::default()
                }
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build the **k8s Service** for a shared service (ADR-0058): a stable cluster
/// DNS name equal to the declared service `name`, so opt-in Pods reach it at
/// `<name>:<port>`. Its selector targets the service Pod's labels.
///
/// Deliberately NOT Take-scoped (unlike the Pod / NetworkPolicy): the object name
/// is the declared DNS name and MUST stay stable across Reruns or `<name>:<port>`
/// would stop resolving for consuming steps. Its `{run, name}` selector matches
/// every Take's service Pod, so as a stale Take's Pod drains the Service simply
/// converges on the live Take's Pod. Recreation across Takes is idempotent
/// (`adopt_conflict`).
pub fn build_service(run: &str, name: &str, namespace: &str, ports: &[u16]) -> Service {
    Service {
        metadata: ObjectMeta {
            // The Service object name IS the declared name → DNS `<name>:<port>`.
            name: Some(sanitize_dns(name)),
            namespace: Some(namespace.to_string()),
            labels: Some(service_labels(run, name)),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(service_labels(run, name)),
            ports: Some(
                ports
                    .iter()
                    .map(|p| ServicePort {
                        port: *p as i32,
                        target_port: Some(IntOrString::Int(*p as i32)),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build the **NetworkPolicy** for a shared service (ADR-0058): default-deny
/// ingress to the service Pod, with a single rule admitting only Pods **in the
/// same run** that carry the service's opt-in label ([`service_uses_label`]).
/// Non-opt-in Pods (and Pods of other runs) cannot reach it — the least-privilege
/// hole `uses:` scopes.
pub fn build_network_policy(
    run: &str,
    name: &str,
    take: i64,
    namespace: &str,
    ports: &[u16],
) -> NetworkPolicy {
    let peer = NetworkPolicyPeer {
        pod_selector: Some(LabelSelector {
            match_labels: Some(std::collections::BTreeMap::from([
                ("scarab.io/run".to_string(), sanitize_label(run)),
                (service_uses_label(name), "true".to_string()),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    };
    NetworkPolicy {
        metadata: ObjectMeta {
            name: Some(service_resource_name(run, name, take)),
            namespace: Some(namespace.to_string()),
            labels: Some(service_labels(run, name)),
            ..Default::default()
        },
        spec: Some(NetworkPolicySpec {
            // Selects the service Pod (the policy target).
            pod_selector: Some(LabelSelector {
                match_labels: Some(service_labels(run, name)),
                ..Default::default()
            }),
            policy_types: Some(vec!["Ingress".to_string()]),
            ingress: Some(vec![NetworkPolicyIngressRule {
                from: Some(vec![peer]),
                ports: Some(
                    ports
                        .iter()
                        .map(|p| NetworkPolicyPort {
                            port: Some(IntOrString::Int(*p as i32)),
                            protocol: Some("TCP".to_string()),
                            ..Default::default()
                        })
                        .collect(),
                ),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The rootless BuildKit image used by a `kind: build` step (ADR-0018).
const BUILDKIT_IMAGE: &str = "moby/buildkit:rootless";
/// Where the registry dockerconfigjson is mounted (`DOCKER_CONFIG` points
/// here); a per-Pod Secret volume — tmpfs on the node, never env (ADR-0018).
const REGISTRY_AUTH_MOUNT_PATH: &str = "/scarab/registry";
const REGISTRY_AUTH_VOLUME: &str = "scarab-registry-auth";
/// The file name inside the mount — what `DOCKER_CONFIG` clients expect.
const REGISTRY_AUTH_KEY: &str = "config.json";

/// The per-Pod Secret carrying the registry dockerconfigjson.
fn registry_secret_name(pod_name: &str) -> String {
    format!("{pod_name}-registry")
}

/// Where the per-attempt OIDC federation token is mounted (ADR-0015):
/// a per-Pod Secret volume (tmpfs on the node), pointed at by
/// `SCARAB_OIDC_TOKEN_FILE` — never env, never argv.
const OIDC_TOKEN_MOUNT_PATH: &str = "/scarab/oidc";
const OIDC_TOKEN_VOLUME: &str = "scarab-oidc-token";
const OIDC_TOKEN_KEY: &str = "token";

/// The per-Pod Secret carrying the OIDC token.
fn oidc_secret_name(pod_name: &str) -> String {
    format!("{pod_name}-oidc")
}

/// The `buildctl` args a build step compiles to (ADR-0018). With the egress
/// sidecar present, the image digest is written to the results volume as the
/// `image` result — the ImageArtifact of record (`containerimage.digest`).
fn buildctl_args(build: &scarab_engine::BuildConfig, with_metadata: bool) -> Vec<String> {
    let mut push_opt = format!("type=image,name={},push={}", build.image, build.push);
    if build.push && build.insecure_push {
        push_opt.push_str(",registry.insecure=true");
    }
    let mut args = vec![
        "build".to_string(),
        "--frontend".into(),
        "dockerfile.v0".into(),
        "--local".into(),
        format!("context={}", build.context),
        "--local".into(),
        format!("dockerfile={}", build.context),
        "--opt".into(),
        format!("filename={}", build.dockerfile),
        "--output".into(),
        push_opt,
    ];
    if with_metadata {
        args.push("--metadata-file".into());
        args.push(format!("{RESULTS_MOUNT_PATH}/image.json"));
    }
    args
}

/// The security context of a build step: rootless BuildKit needs a non-root
/// uid with an unconfined seccomp profile (user-namespace worker) — explicitly
/// NOT privileged, and never the author's request (a build step cannot ask
/// for escalation; the pipeline compiler rejects it).
fn build_security_context() -> SecurityContext {
    SecurityContext {
        privileged: Some(false),
        allow_privilege_escalation: Some(true), // setuid newuidmap needs it
        run_as_non_root: Some(true),
        run_as_user: Some(1000),
        seccomp_profile: Some(SeccompProfile {
            type_: "Unconfined".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The dockerconfigjson for a build Pod (ADR-0018): the scoped secret's JSON
/// verbatim when present, else synthesized from the forge-derived credential.
fn registry_dockerconfig(build: &scarab_engine::BuildConfig) -> Option<String> {
    if let Some(json) = &build.registry_auth_json {
        return Some(json.clone());
    }
    build.derived_auth.as_ref().map(|cred| {
        use base64::Engine;
        let auth = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", cred.username, cred.token));
        serde_json::json!({ "auths": { cred.registry.clone(): { "auth": auth } } }).to_string()
    })
}

/// The image an image-build step produced, recorded as an Artifact of record
/// (ADR-0018): the pushed reference and its content digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageArtifact {
    pub image: String,
    pub digest: String,
}

/// Record a built image's digest as an [`ImageArtifact`].
pub fn image_artifact(build: &scarab_engine::BuildConfig, digest: &str) -> ImageArtifact {
    ImageArtifact {
        image: build.image.clone(),
        digest: digest.to_string(),
    }
}

/// The idempotency key that neutralizes a double push of the same content
/// (ADR-0021): pushing `image@digest` twice is one logical effect.
pub fn push_fence(image: &str, digest: &str) -> String {
    format!("push:{image}@{digest}")
}

/// Map a Pod's observed status to the domain [`ExecState`]. For a
/// `restartPolicy: Never` Pod the phase is terminal on completion; the exit code
/// is lifted from the container's terminated state, and every terminal failure
/// carries a [`FailureClass`] (ADR-0047) — only this adapter sees the execution
/// conditions around the opaque step, so only it can classify.
pub fn pod_state(pod: &Pod) -> ExecState {
    let phase = pod
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("Pending");
    match phase {
        "Succeeded" => ExecState::Succeeded,
        "Failed" => {
            let exit_code = container_exit_code(pod);
            // ADR-0061 s3-feed: the fetcher can FAIL, where the `busybox` doorstop
            // it replaced structurally could not. A Step whose workspace was never
            // provisioned has no step-container verdict at all, so without this the
            // Pod would fall into the "no verdict yet, defer" branch below and stay
            // `Pending` until the engine's timeout backstop — an hour, by default,
            // for a failure we already know about.
            //
            // `Infra { never_started: true }` is deliberately the SAME verdict the
            // deleted `DriveErr::InputMissing` produced: the Step's main process
            // never ran, so no side effect is possible, and a bounded retry may
            // land on a node that can reach the service. A permanently-missing
            // snapshot simply exhausts that budget and dead-letters, which is what
            // it did before.
            if let Some(fetch_exit) = workspace_fetch_failed(pod) {
                if exit_code.is_none() {
                    return ExecState::Failed {
                        exit_code: None,
                        class: FailureClass::Infra {
                            never_started: true,
                        },
                        cause: Some(format!(
                            "workspace fetch init container failed (exit {fetch_exit}) — \
                             the step's inputs were never provisioned (ADR-0061 s3-feed)"
                        )),
                    };
                }
            }
            // A just-killed Pod can surface `phase: Failed` BEFORE the kubelet
            // finishes writing the verdict — no status reason, no terminated
            // container state (observed on k3s enforcing
            // activeDeadlineSeconds). Classifying that snapshot would misread
            // a timeout as never-started infra. A Failed phase never reverts,
            // so defer: report Pending and classify on the next poll, when the
            // settled status (e.g. `reason: DeadlineExceeded`) has landed. The
            // engine's timeout backstop bounds the pathological
            // never-settling case.
            let reason = pod.status.as_ref().and_then(|s| s.reason.as_deref());
            // `ContainerStatusUnknown` means the kubelet could not locate the
            // container when the Pod was killed — its exit code (137) is
            // synthesized, not the step's verdict.
            let synthetic =
                step_terminated_reason(pod).as_deref() == Some("ContainerStatusUnknown");
            let no_verdict = exit_code.is_none() && step_terminated_reason(pod).is_none();
            if reason.is_none() && (no_verdict || synthetic) {
                return ExecState::Pending;
            }
            ExecState::Failed {
                exit_code,
                class: classify_failed_pod(pod, exit_code),
                cause: None,
            }
        }
        "Running" => ExecState::Running,
        // "Unknown" means the node stopped reporting — the backend lost it.
        // Conservatively post-start (ADR-0047): can't prove it never ran.
        "Unknown" => ExecState::Lost,
        // A Pending Pod is normally still scheduling or pulling — but some
        // states are terminal-while-Pending: a container `waiting` reason the
        // kubelet can never recover from (bad config, unpullable image), or a
        // scheduler verdict of Unschedulable. Such a Pod stays Pending forever,
        // hanging the run, so surface it now (ADR-0047). The main process never
        // ran, so no side effect is possible; the class splits by whether the
        // rejection is permanent (`Config`, fail fast) or possibly transient
        // (`Infra { never_started }`, bounded auto-retry).
        _ => {
            // Also checked while Pending: an init container that exited non-zero
            // under `restartPolicy: Never` is never retried by the kubelet, so
            // there is nothing to wait for even before the phase flips to Failed.
            if let Some(fetch_exit) = workspace_fetch_failed(pod) {
                ExecState::Failed {
                    exit_code: None,
                    class: FailureClass::Infra {
                        never_started: true,
                    },
                    cause: Some(format!(
                        "workspace fetch init container failed (exit {fetch_exit}) — \
                         the step's inputs were never provisioned (ADR-0061 s3-feed)"
                    )),
                }
            } else if let Some(class) = terminal_waiting_class(pod) {
                ExecState::Failed {
                    exit_code: None,
                    class,
                    cause: None,
                }
            } else if is_unschedulable(pod) {
                ExecState::Failed {
                    exit_code: None,
                    class: FailureClass::Infra {
                        never_started: true,
                    },
                    cause: None,
                }
            } else {
                ExecState::Pending
            }
        }
    }
}

/// Classify a `phase: Failed` Pod (ADR-0047).
///
/// Order matters: the kubelet's own verdicts (deadline, eviction, OOM-kill)
/// take precedence over the container exit code — an OOM-killed container also
/// reports exit 137, but the *platform* killed it, not the step's own logic.
fn classify_failed_pod(pod: &Pod, exit_code: Option<i32>) -> FailureClass {
    let pod_reason = pod.status.as_ref().and_then(|s| s.reason.as_deref());
    match pod_reason {
        // activeDeadlineSeconds elapsed — the kubelet killed the Pod.
        Some("DeadlineExceeded") => return FailureClass::Timeout,
        // Evicted: infra reclaimed the Pod. Whether a side effect is possible
        // depends on whether the step's process had started.
        Some("Evicted") => {
            return FailureClass::Infra {
                never_started: !step_container_started(pod),
            }
        }
        _ => {}
    }
    // The platform OOM-killed the started process: post-start infra.
    if step_terminated_reason(pod).as_deref() == Some("OOMKilled") {
        return FailureClass::Infra {
            never_started: false,
        };
    }
    // `reason: DeadlineExceeded` can propagate a beat AFTER the kubelet kills
    // the containers (observed on k3s: the first Failed observation carries
    // only the SIGKILL exit 137). The Pod itself is authoritative: if it
    // outlived its own spec'd deadline, the kill is a timeout — never the
    // step's verdict.
    if outlived_deadline(pod) {
        return FailureClass::Timeout;
    }
    match exit_code {
        // The step's own code produced a verdict.
        Some(_) => FailureClass::Step,
        // Failed with no exit code: the container never produced a verdict —
        // infra, with side-effect possibility keyed on whether it started.
        None => FailureClass::Infra {
            never_started: !step_container_started(pod),
        },
    }
}

/// True if the Pod ran past its own `activeDeadlineSeconds` — the
/// deterministic timeout signal, independent of the (sometimes-late)
/// `DeadlineExceeded` status reason.
fn outlived_deadline(pod: &Pod) -> bool {
    let Some(deadline) = pod.spec.as_ref().and_then(|s| s.active_deadline_seconds) else {
        return false;
    };
    let Some(start) = pod.status.as_ref().and_then(|s| s.start_time.as_ref()) else {
        return false;
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(i64::MAX);
    now_secs - start.0.as_second() >= deadline
}

/// True once the step container's main process has (ever) started: a running
/// or terminated state now, a prior terminated state, or the kubelet's
/// `started` flag. Point-in-time observations of an evicted/failed Pod retain
/// these fields, so this distinguishes never-started from post-start.
fn step_container_started(pod: &Pod) -> bool {
    let Some(c) = step_container_status(pod) else {
        return false;
    };
    c.started == Some(true)
        || c.state
            .as_ref()
            .is_some_and(|s| s.running.is_some() || s.terminated.is_some())
        || c.last_state
            .as_ref()
            .is_some_and(|s| s.terminated.is_some())
}

/// The workspace fetcher's non-zero exit code, if it failed (ADR-0061 s3-feed).
///
/// Scoped to `scarab-workspace-init` **by name**, and that narrowness is the
/// point: the other entries in `init_container_statuses` are *native sidecars*
/// (the egress barrier, the results sidecar, `service-*`), which the kubelet
/// terminates when the Pod ends and which routinely exit non-zero (137) while the
/// Step itself succeeded or failed on its own terms. Treating any non-zero init
/// container as a provisioning failure would report `never_started: true` for a
/// Step that had in fact run — the exact misclassification `never_started` exists
/// to prevent (ADR-0047: it is what tells the engine whether a side effect was
/// possible).
///
/// `last_state` is consulted too, so a status snapshot taken after the Pod moved
/// on still yields the verdict.
fn workspace_fetch_failed(pod: &Pod) -> Option<i32> {
    pod.status
        .as_ref()?
        .init_container_statuses
        .as_ref()?
        .iter()
        .find(|c| c.name == WORKSPACE_INIT_CONTAINER)
        .and_then(|c| {
            c.state
                .as_ref()
                .and_then(|s| s.terminated.as_ref())
                .or_else(|| c.last_state.as_ref().and_then(|s| s.terminated.as_ref()))
        })
        .map(|t| t.exit_code)
        .filter(|code| *code != 0)
}

/// The step container's terminated `reason` (e.g. `OOMKilled`), current or last.
fn step_terminated_reason(pod: &Pod) -> Option<String> {
    let c = step_container_status(pod)?;
    c.state
        .as_ref()
        .and_then(|s| s.terminated.as_ref())
        .or_else(|| c.last_state.as_ref().and_then(|s| s.terminated.as_ref()))
        .and_then(|t| t.reason.clone())
}

/// The step container's status — by name, falling back to the first (mirrors
/// [`container_exit_code`]'s selection).
fn step_container_status(pod: &Pod) -> Option<&k8s_openapi::api::core::v1::ContainerStatus> {
    let statuses = pod.status.as_ref()?.container_statuses.as_ref()?;
    statuses
        .iter()
        .find(|c| c.name == STEP_CONTAINER)
        .or_else(|| statuses.first())
}

/// True if the scheduler has verdict-ed the Pod `Unschedulable`
/// (`PodScheduled=False`). The Pod would otherwise sit Pending indefinitely;
/// ADR-0047 treats it as never-started infra churn to self-heal via bounded
/// auto-retry.
fn is_unschedulable(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|c| {
            c.type_ == "PodScheduled"
                && c.status == "False"
                && c.reason.as_deref() == Some("Unschedulable")
        })
}

/// Classify a container (step or native sidecar) stuck in a `waiting` state the
/// kubelet cannot recover from on its own — these keep the Pod `Pending`
/// indefinitely, so we treat them as terminal rather than waiting forever.
/// Returns `None` when no such reason is present (the Pod is legitimately still
/// scheduling / pulling / initializing).
///
/// The split drives retry policy (ADR-0047):
/// - **Config rejection** (bad securityContext, invalid image name, container-
///   config error) is permanent — re-running the identical spec can never
///   succeed — so it is `Config`: fail fast with a developer verdict, no retry.
/// - **Image-pull** failure can be a transient registry/network blip, so it
///   stays `Infra { never_started }` (bounded auto-retry). A genuinely absent
///   image simply exhausts that budget and dead-letters — an operator concern.
///
/// A config reason is definitive and wins over a co-occurring pull reason.
fn terminal_waiting_class(pod: &Pod) -> Option<FailureClass> {
    const CONFIG: &[&str] = &[
        "CreateContainerConfigError",
        "CreateContainerError",
        "RunContainerError",
        "InvalidImageName",
    ];
    const IMAGE_PULL: &[&str] = &["ErrImagePull", "ImagePullBackOff"];
    let status = pod.status.as_ref()?;
    let mut class = None;
    for reason in status
        .container_statuses
        .iter()
        .flatten()
        .chain(status.init_container_statuses.iter().flatten())
        .filter_map(|c| c.state.as_ref()?.waiting.as_ref()?.reason.as_deref())
    {
        if CONFIG.contains(&reason) {
            return Some(FailureClass::Config);
        }
        if IMAGE_PULL.contains(&reason) {
            class = Some(FailureClass::Infra {
                never_started: true,
            });
        }
    }
    class
}

fn container_exit_code(pod: &Pod) -> Option<i32> {
    // The step's exit code — explicitly the `step` container, never the egress
    // sidecar (ADR-0042). The sidecar is a native sidecar (initContainer), so it
    // is not in `container_statuses`, but selecting by name keeps this correct
    // regardless of ordering or future containers.
    pod.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .iter()
        .find(|c| c.name == STEP_CONTAINER)
        .or_else(|| {
            pod.status
                .as_ref()
                .unwrap()
                .container_statuses
                .as_ref()
                .unwrap()
                .first()
        })?
        .state
        .as_ref()?
        .terminated
        .as_ref()
        .map(|t| t.exit_code)
}

/// Lowercase, DNS-1123-label-safe slug: `[a-z0-9-]`, collapsed dashes, trimmed.
fn sanitize_dns(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// A label *value*: like a DNS slug but bounded to 63 chars.
fn sanitize_label(s: &str) -> String {
    truncate(&sanitize_dns(s), 63)
}

fn truncate(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// FNV-1a 32-bit — a tiny, fully deterministic hash (no `std` RNG variance), so
/// Pod names are stable across builds and restarts.
fn fnv1a(s: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_engine::{Attempt, AttemptId, AttemptOutcome, RunId, StepId, StepStatus, Timestamp};

    fn step_with_attempt(run: &str, step: &str, attempt: &str) -> StepRun {
        StepRun {
            run: RunId(run.into()),
            step: StepId(step.into()),
            status: StepStatus::Running,
            attempts: vec![Attempt {
                id: AttemptId(attempt.into()),
                started_at: Timestamp(0),
                failure: None,
                failure_detail: None,
                output_durability: None,
                outcome: AttemptOutcome::Running,
            }],
            needs: vec![],
            gate_kind: None,
        }
    }

    fn busybox() -> StepSpec {
        StepSpec {
            image: "busybox:latest".into(),
            command: vec!["echo".into(), "hi".into()],
            env: vec![("FOO".into(), "bar".into())],
            secrets: vec![],
            run_as_root: false,
            add_capabilities: vec![],
            privileged: false,
            timeout_seconds: None,
            workspace_inputs: vec![],
            workspace_outputs: vec![],
            clone: None,
            build: None,
            artifacts: vec![],
            placement_profiles: vec![],
            resources: Default::default(),
            k8s_overlay: None,
            oidc_token: None,
            services: vec![],
            uses: vec![],
            matrix_values: Default::default(),
        }
    }

    fn profile(name: &str, k8s: serde_json::Value) -> scarab_pipeline::PlacementProfile {
        scarab_pipeline::PlacementProfile {
            name: name.into(),
            default: false,
            k8s: Some(k8s),
        }
    }

    fn pod_for(spec: &StepSpec) -> Pod {
        let step = step_with_attempt("run-1", "s", "a1");
        build_pod(
            "scarab-x",
            "ns",
            &step,
            spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        )
    }

    #[test]
    fn placement_baseline_is_stamped_on_every_pod() {
        let spec = busybox(); // no placement fields at all
        let placement = PlacementConfig {
            baseline: Some(serde_json::json!({
                "spec": { "tolerations": [
                    {"key":"workload-type","operator":"Equal","value":"application-sub-critical","effect":"NoSchedule"}
                ]}
            })),
            ..Default::default()
        };
        let ps = apply_placement(pod_for(&spec), &spec, &placement)
            .unwrap()
            .spec
            .unwrap();
        let tol = &ps.tolerations.unwrap()[0];
        assert_eq!(tol.key.as_deref(), Some("workload-type"));
        assert_eq!(tol.value.as_deref(), Some("application-sub-critical"));
    }

    #[test]
    fn placement_profiles_merge_in_listed_order() {
        let mut spec = busybox();
        spec.placement_profiles = vec!["arm64".into(), "critical".into()];
        let placement = PlacementConfig {
            profiles: vec![
                profile(
                    "arm64",
                    serde_json::json!({"spec":{"nodeSelector":{"kubernetes.io/arch":"arm64"}}}),
                ),
                profile(
                    "critical",
                    serde_json::json!({"spec":{"tolerations":[
                    {"key":"workload-type","operator":"Equal","value":"application-critical","effect":"NoSchedule"}]}}),
                ),
            ],
            ..Default::default()
        };
        let ps = apply_placement(pod_for(&spec), &spec, &placement)
            .unwrap()
            .spec
            .unwrap();
        assert_eq!(
            ps.node_selector
                .unwrap()
                .get("kubernetes.io/arch")
                .map(String::as_str),
            Some("arm64")
        );
        assert_eq!(
            ps.tolerations.unwrap()[0].value.as_deref(),
            Some("application-critical")
        );
    }

    #[test]
    fn placement_resources_go_on_step_container_as_guaranteed_qos() {
        let mut spec = busybox();
        spec.resources = scarab_pipeline::Resources {
            cpu_millis: Some(8000),
            memory_mib: Some(16384),
        };
        let ps = apply_placement(pod_for(&spec), &spec, &PlacementConfig::default())
            .unwrap()
            .spec
            .unwrap();
        let c = ps
            .containers
            .into_iter()
            .find(|c| c.name == STEP_CONTAINER)
            .unwrap();
        let r = c.resources.unwrap();
        assert_eq!(r.requests.as_ref().unwrap()["cpu"].0, "8000m");
        assert_eq!(r.limits.as_ref().unwrap()["memory"].0, "16384Mi");
    }

    #[test]
    fn placement_default_resources_used_when_step_requests_none() {
        let spec = busybox();
        let placement = PlacementConfig {
            default_resources: scarab_pipeline::Resources {
                cpu_millis: Some(1000),
                memory_mib: Some(2048),
            },
            ..Default::default()
        };
        let ps = apply_placement(pod_for(&spec), &spec, &placement)
            .unwrap()
            .spec
            .unwrap();
        let c = ps
            .containers
            .into_iter()
            .find(|c| c.name == STEP_CONTAINER)
            .unwrap();
        assert_eq!(c.resources.unwrap().requests.unwrap()["cpu"].0, "1000m");
    }

    #[test]
    fn unknown_placement_profile_is_fail_closed() {
        let mut spec = busybox();
        spec.placement_profiles = vec!["ghost".into()];
        let err = apply_placement(pod_for(&spec), &spec, &PlacementConfig::default()).unwrap_err();
        assert!(
            err.contains("unknown placement_profile `ghost`"),
            "got: {err}"
        );
    }

    #[test]
    fn k8s_overlay_wins_last() {
        let mut spec = busybox();
        spec.k8s_overlay = Some(serde_json::json!({"spec":{"schedulerName":"mine"}}));
        let placement = PlacementConfig {
            baseline: Some(serde_json::json!({"spec":{"schedulerName":"default-sched"}})),
            ..Default::default()
        };
        let ps = apply_placement(pod_for(&spec), &spec, &placement)
            .unwrap()
            .spec
            .unwrap();
        assert_eq!(ps.scheduler_name.as_deref(), Some("mine"));
    }

    #[test]
    fn empty_placement_is_a_noop() {
        let spec = busybox();
        let pod = pod_for(&spec);
        let before = serde_json::to_value(&pod).unwrap();
        let after =
            serde_json::to_value(apply_placement(pod, &spec, &PlacementConfig::default()).unwrap())
                .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn step_pod_is_hardened_restricted_by_default() {
        let step = step_with_attempt("run-1", "build", "a1");
        let pod = build_pod(
            "scarab-x",
            "scarab-run-1",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        let sc = pod.spec.unwrap().containers[0]
            .security_context
            .clone()
            .expect("baseline security context must be set");
        // ADR-0039 restricted floor.
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(sc.run_as_user, None);
        assert_eq!(sc.privileged, Some(false));
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        assert_eq!(
            sc.capabilities.as_ref().unwrap().drop,
            Some(vec!["ALL".to_string()])
        );
        assert!(sc.capabilities.as_ref().unwrap().add.is_none());
        assert_eq!(sc.seccomp_profile.unwrap().type_, "RuntimeDefault");
    }

    #[test]
    fn admitted_grants_are_applied_as_exceptions() {
        let step = step_with_attempt("run-1", "build", "a1");
        let spec = StepSpec {
            image: "ghcr.io/acme/deployer@sha256:aaaa".into(),
            command: vec![],
            env: vec![],
            secrets: vec![],
            run_as_root: true,
            add_capabilities: vec!["NET_ADMIN".into()],
            privileged: true,
            timeout_seconds: None,
            workspace_inputs: vec![],
            workspace_outputs: vec![],
            clone: None,
            build: None,
            artifacts: vec![],
            placement_profiles: vec![],
            resources: Default::default(),
            k8s_overlay: None,
            oidc_token: None,
            services: vec![],
            uses: vec![],
            matrix_values: Default::default(),
        };
        let pod = build_pod(
            "scarab-x",
            "scarab-run-1",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        let sc = pod.spec.unwrap().containers[0]
            .security_context
            .clone()
            .unwrap();
        assert_eq!(sc.run_as_non_root, Some(false));
        assert_eq!(sc.run_as_user, Some(0));
        assert_eq!(sc.privileged, Some(true));
        assert_eq!(sc.allow_privilege_escalation, Some(true));
        // Still drops ALL, then adds only the admitted capability.
        let caps = sc.capabilities.unwrap();
        assert_eq!(caps.drop, Some(vec!["ALL".to_string()]));
        assert_eq!(caps.add, Some(vec!["NET_ADMIN".to_string()]));
    }

    #[test]
    fn run_as_root_alone_does_not_enable_privilege_escalation() {
        let step = step_with_attempt("run-1", "build", "a1");
        let spec = StepSpec {
            run_as_root: true,
            ..busybox()
        };
        let pod = build_pod(
            "scarab-x",
            "scarab-run-1",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        let sc = pod.spec.unwrap().containers[0]
            .security_context
            .clone()
            .unwrap();
        assert_eq!(sc.run_as_non_root, Some(false));
        assert_eq!(sc.privileged, Some(false));
        // Self-service root stays unprivileged and non-escalating.
        assert_eq!(sc.allow_privilege_escalation, Some(false));
    }

    // ADR-0058: a declared sidecar service is injected as a native sidecar
    // (initContainer, restartPolicy Always) co-located in the step's Pod, with
    // its readiness probe wired as the container's startupProbe so the kubelet
    // holds the MAIN step container until the service is ready.
    #[test]
    fn sidecar_service_is_colocated_with_wired_startup_probe() {
        use scarab_pipeline::{ReadyProbe, ServiceSpec};
        let mut spec = busybox();
        spec.services = vec![ServiceSpec {
            image: "postgres:16".into(),
            env: std::collections::BTreeMap::from([(
                "POSTGRES_PASSWORD".to_string(),
                "test".to_string(),
            )]),
            ports: vec![5432],
            ready: Some(ReadyProbe {
                tcp: Some(5432),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let pod = pod_for(&spec);
        let ps = pod.spec.unwrap();

        // The MAIN step container is untouched and stays a single app container.
        assert_eq!(ps.containers.len(), 1);
        assert_eq!(ps.containers[0].name, STEP_CONTAINER);

        // The service rides as a native sidecar: an initContainer with
        // restartPolicy Always (co-located, localhost-reachable).
        let inits = ps.init_containers.expect("service sidecar injected");
        let svc = inits
            .iter()
            .find(|c| c.name == "service-0")
            .expect("service-0 sidecar present");
        assert_eq!(svc.image.as_deref(), Some("postgres:16"));
        assert_eq!(svc.restart_policy.as_deref(), Some("Always"));
        assert_eq!(svc.ports.as_ref().unwrap()[0].container_port, 5432);
        assert_eq!(
            svc.env.as_ref().unwrap()[0].name.as_str(),
            "POSTGRES_PASSWORD"
        );

        // Readiness → the sidecar's startupProbe (a TCP-connect on 5432): this is
        // what gates the main container start until the service is ready.
        let probe = svc.startup_probe.as_ref().expect("startup probe wired");
        let tcp = probe.tcp_socket.as_ref().expect("tcp probe");
        assert_eq!(tcp.port, IntOrString::Int(5432));

        // Governance (ADR-0039): the sidecar inherits the restricted baseline.
        let sc = svc.security_context.as_ref().unwrap();
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(sc.privileged, Some(false));
        assert_eq!(
            sc.capabilities.as_ref().unwrap().drop,
            Some(vec!["ALL".to_string()])
        );
    }

    // ADR-0058: a sidecar's `run-as-root` is the service's OWN self-service grant
    // (independent of the step's) — stock DB images that genuinely need root opt in
    // on the service, not the step. The default probe is a TCP-connect on the first
    // declared port when `ready:` is omitted.
    #[test]
    fn sidecar_run_as_root_is_the_services_own_grant_and_defaults_probe_to_first_port() {
        use scarab_pipeline::ServiceSpec;
        let mut spec = busybox();
        // Step stays baseline non-root; the SERVICE opts into root on its own.
        spec.services = vec![ServiceSpec {
            image: "redis:7".into(),
            ports: vec![6379],
            ready: None, // default → TCP on the first declared port
            run_as_root: true,
            ..Default::default()
        }];
        let inits = pod_for(&spec).spec.unwrap().init_containers.unwrap();
        let svc = inits.iter().find(|c| c.name == "service-0").unwrap();
        let sc = svc.security_context.as_ref().unwrap();
        assert_eq!(sc.run_as_non_root, Some(false));
        assert_eq!(sc.run_as_user, Some(0));
        let tcp = svc
            .startup_probe
            .as_ref()
            .unwrap()
            .tcp_socket
            .as_ref()
            .unwrap();
        assert_eq!(tcp.port, IntOrString::Int(6379));
    }

    // ADR-0058 (git-bug d2301fb): a service is **non-root by default**. Pinning the
    // image's built-in non-root uid via `run_as_user` sets the container
    // runAsUser/runAsGroup AND the Pod-level fsGroup, so the service's emptyDir data
    // volume is group-writable (stock postgres writes PGDATA without root).
    #[test]
    fn sidecar_run_as_user_pins_uid_and_sets_pod_fs_group() {
        use scarab_pipeline::ServiceSpec;
        let mut spec = busybox();
        spec.services = vec![ServiceSpec {
            image: "postgres:16".into(),
            ports: vec![5432],
            run_as_user: Some(999),
            ..Default::default()
        }];
        let ps = pod_for(&spec).spec.unwrap();
        let inits = ps.init_containers.as_ref().unwrap();
        let svc = inits.iter().find(|c| c.name == "service-0").unwrap();
        let sc = svc.security_context.as_ref().unwrap();
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(sc.run_as_user, Some(999));
        assert_eq!(sc.run_as_group, Some(999));
        // Pod-level fsGroup makes the (shared) Pod's data emptyDir group-writable.
        assert_eq!(
            ps.security_context.as_ref().unwrap().fs_group,
            Some(999),
            "the service's non-root gid becomes the Pod fsGroup"
        );
    }

    // ADR-0058 (git-bug d2301fb): the `run_as_root` escape hatch runs a shared
    // service Pod as uid 0 (no fsGroup needed — root writes anything), while the
    // non-root default with a pinned uid gets a matching Pod fsGroup.
    #[test]
    fn shared_service_pod_honors_run_as_user_and_run_as_root() {
        use scarab_pipeline::ServiceSpec;
        // Non-root default with a pinned uid → runAsUser/Group + Pod fsGroup.
        let non_root = ServiceSpec {
            image: "postgres:16".into(),
            ports: vec![5432],
            run_as_user: Some(999),
            ..Default::default()
        };
        let pod = build_service_pod("run-1", "db", 1, "ns", &non_root);
        let ps = pod.spec.unwrap();
        let sc = ps.containers[0].security_context.as_ref().unwrap();
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(sc.run_as_user, Some(999));
        assert_eq!(sc.run_as_group, Some(999));
        assert_eq!(ps.security_context.as_ref().unwrap().fs_group, Some(999));

        // Escape hatch: run as root, no Pod fsGroup.
        let root = ServiceSpec {
            image: "legacy:1".into(),
            ports: vec![5432],
            run_as_root: true,
            ..Default::default()
        };
        let pod = build_service_pod("run-1", "db", 1, "ns", &root);
        let ps = pod.spec.unwrap();
        let sc = ps.containers[0].security_context.as_ref().unwrap();
        assert_eq!(sc.run_as_non_root, Some(false));
        assert_eq!(sc.run_as_user, Some(0));
        assert!(ps.security_context.is_none(), "root needs no fsGroup");
    }

    // ADR-0058 (shared service): the standalone service Pod runs the service
    // image as its single main container under the restricted baseline, with the
    // `ready:` probe wired as the container's readinessProbe (so the k8s Service
    // only routes once ready), and carries the run + service labels.
    #[test]
    fn shared_service_pod_is_standalone_hardened_with_readiness_probe() {
        use scarab_pipeline::{ReadyProbe, ServiceSpec};
        let svc = ServiceSpec {
            image: "postgres:16".into(),
            env: std::collections::BTreeMap::from([(
                "POSTGRES_PASSWORD".to_string(),
                "test".to_string(),
            )]),
            ports: vec![5432],
            ready: Some(ReadyProbe {
                tcp: Some(5432),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pod = build_service_pod("run-1", "db", 1, "ns", &svc);
        let meta = &pod.metadata;
        assert_eq!(meta.namespace.as_deref(), Some("ns"));
        let labels = meta.labels.as_ref().unwrap();
        assert_eq!(
            labels.get("scarab.io/run").map(String::as_str),
            Some("run-1")
        );
        assert_eq!(
            labels.get("scarab.io/service").map(String::as_str),
            Some("db")
        );

        let ps = pod.spec.unwrap();
        // Single standalone container (NOT an init/sidecar).
        assert_eq!(ps.containers.len(), 1);
        assert!(ps.init_containers.is_none());
        let c = &ps.containers[0];
        assert_eq!(c.image.as_deref(), Some("postgres:16"));
        // readinessProbe (not startupProbe) gates the Service endpoint.
        let tcp = c
            .readiness_probe
            .as_ref()
            .expect("readiness probe")
            .tcp_socket
            .as_ref()
            .unwrap();
        assert_eq!(tcp.port, IntOrString::Int(5432));
        // Restricted baseline (ADR-0039).
        let sc = c.security_context.as_ref().unwrap();
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(sc.privileged, Some(false));
        assert_eq!(
            sc.capabilities.as_ref().unwrap().drop,
            Some(vec!["ALL".to_string()])
        );
    }

    // ADR-0058: the k8s Service is named for the declared service (cluster DNS
    // `<name>:<port>`) and selects the service Pod by its labels.
    #[test]
    fn shared_service_object_gives_dns_name_and_selects_the_pod() {
        let svc = build_service("run-1", "db", "ns", &[5432]);
        assert_eq!(
            svc.metadata.name.as_deref(),
            Some("db"),
            "DNS name = service name"
        );
        let sp = svc.spec.unwrap();
        let sel = sp.selector.unwrap();
        assert_eq!(sel.get("scarab.io/service").map(String::as_str), Some("db"));
        assert_eq!(sel.get("scarab.io/run").map(String::as_str), Some("run-1"));
        let port = &sp.ports.unwrap()[0];
        assert_eq!(port.port, 5432);
        assert_eq!(port.target_port, Some(IntOrString::Int(5432)));
    }

    // ADR-0058: the NetworkPolicy targets the service Pod and admits ingress ONLY
    // from same-run Pods carrying the service's opt-in label — the least-privilege
    // hole `uses:` scopes. A Pod that opts into nothing is denied.
    #[test]
    fn shared_service_network_policy_admits_only_opt_in_pods() {
        let np = build_network_policy("run-1", "db", 1, "ns", &[5432]);
        let spec = np.spec.unwrap();
        // Target = the service Pod.
        let target = spec.pod_selector.unwrap().match_labels.unwrap();
        assert_eq!(
            target.get("scarab.io/service").map(String::as_str),
            Some("db")
        );
        assert_eq!(
            spec.policy_types.as_deref(),
            Some(&["Ingress".to_string()][..])
        );
        // Ingress source peer = same-run Pods with the db opt-in label.
        let rule = &spec.ingress.unwrap()[0];
        let peer_sel = rule.from.as_ref().unwrap()[0]
            .pod_selector
            .as_ref()
            .unwrap()
            .match_labels
            .as_ref()
            .unwrap();
        assert_eq!(
            peer_sel.get("scarab.io/run").map(String::as_str),
            Some("run-1")
        );
        assert_eq!(
            peer_sel.get("scarab.io/uses.db").map(String::as_str),
            Some("true"),
            "only Pods opted into `db` are admitted"
        );
        assert_eq!(
            rule.ports.as_ref().unwrap()[0].port,
            Some(IntOrString::Int(5432))
        );
    }

    // ADR-0058 (Rerun collision fix): a shared service's Pod / NetworkPolicy name
    // is Take-scoped (`{run, name, take}`), so a Rerun's fresh Take never reuses
    // the prior (still-terminating) Take's name in this single-namespace executor.
    // Distinct Takes -> distinct names; same Take -> stable name (launch re-attach);
    // the name stays DNS-1123-safe and ≤63 chars even for a long service name and
    // a large Take.
    #[test]
    fn service_resource_name_is_take_scoped_and_dns_safe() {
        // Same {run, name} but different Take must NOT collide.
        let t1 = service_resource_name("run-1", "postgres", 1);
        let t2 = service_resource_name("run-1", "postgres", 2);
        assert_ne!(t1, t2, "a Rerun's Take must get a distinct resource name");

        // Same {run, name, take} is stable (idempotent launch re-attaches).
        assert_eq!(t1, service_resource_name("run-1", "postgres", 1));

        // The Pod and NetworkPolicy builders agree on the Take-scoped name so
        // teardown-by-handle targets the right instance.
        let pod = build_service_pod("run-1", "postgres", 2, "ns", &Default::default());
        let np = build_network_policy("run-1", "postgres", 2, "ns", &[5432]);
        assert_eq!(pod.metadata.name.as_deref(), Some(t2.as_str()));
        assert_eq!(np.metadata.name.as_deref(), Some(t2.as_str()));

        // DNS-1123 label safety + ≤63 for a pathological name and a huge Take.
        let long = "a-really-long-service-name-that-exceeds-the-slug-budget-considerably";
        let n = service_resource_name("some-run-id", long, i64::MAX);
        assert!(
            n.len() <= 63,
            "name {n:?} exceeds the 63-char DNS label limit"
        );
        assert!(
            n.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "name {n:?} is not DNS-1123-label-safe"
        );
        assert!(
            !n.starts_with('-') && !n.ends_with('-'),
            "name {n:?} has a dangling dash"
        );
        // Even when the readable suffix is clipped, folding the Take into the hash
        // keeps different Takes distinct.
        assert_ne!(n, service_resource_name("some-run-id", long, i64::MAX - 1));
    }

    // ADR-0058: an opt-in step's Pod carries the per-service `uses` label so the
    // service NetworkPolicy admits it; a step that opts into nothing does not.
    #[test]
    fn step_pod_carries_uses_opt_in_labels() {
        let mut spec = busybox();
        spec.uses = vec!["db".to_string()];
        let labels = pod_for(&spec).metadata.labels.unwrap();
        assert_eq!(
            labels.get("scarab.io/uses.db").map(String::as_str),
            Some("true")
        );

        let plain = busybox();
        let plain_labels = pod_for(&plain).metadata.labels.unwrap();
        assert!(!plain_labels
            .keys()
            .any(|k| k.starts_with("scarab.io/uses.")));
    }

    #[test]
    fn pod_name_is_deterministic_per_fence() {
        let a = step_with_attempt("run-1", "build", "a1");
        // Same fence -> same name (this is what lets launch re-attach).
        assert_eq!(
            pod_name(&a),
            pod_name(&step_with_attempt("run-1", "build", "a1"))
        );
        // Different attempt -> different name.
        assert_ne!(
            pod_name(&a),
            pod_name(&step_with_attempt("run-1", "build", "a2"))
        );
        // Different step -> different name.
        assert_ne!(
            pod_name(&a),
            pod_name(&step_with_attempt("run-1", "test", "a1"))
        );

        let name = pod_name(&a);
        assert!(name.starts_with("scarab-"));
        assert!(name.len() <= 63, "name must be a valid DNS-1123 label");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "name {name} must be DNS-1123-safe"
        );
    }

    #[test]
    fn pod_name_sanitizes_messy_ids() {
        let s = step_with_attempt("Run/With CAPS", "step_under.score", "att#1");
        let name = pod_name(&s);
        assert!(name.starts_with("scarab-"));
        assert!(name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert!(!name.contains("--"));
    }

    #[test]
    fn build_pod_sets_image_command_restart_policy_and_fence_env() {
        let step = step_with_attempt("run-1", "build", "a1");
        let pod = build_pod(
            "scarab-build-a1-deadbeef",
            "scarab-run-1",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );

        let spec = pod.spec.unwrap();
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
        let c = &spec.containers[0];
        assert_eq!(c.image.as_deref(), Some("busybox:latest"));
        assert_eq!(
            c.command.as_ref().unwrap(),
            &vec!["echo".to_string(), "hi".to_string()]
        );

        let env = c.env.as_ref().unwrap();
        let get = |k: &str| {
            env.iter()
                .find(|e| e.name == k)
                .and_then(|e| e.value.clone())
        };
        assert_eq!(get("FOO").as_deref(), Some("bar")); // spec env preserved
        assert_eq!(get("SCARAB_RUN").as_deref(), Some("run-1")); // fence injected
        assert_eq!(get("SCARAB_STEP").as_deref(), Some("build"));
        assert_eq!(get("SCARAB_ATTEMPT").as_deref(), Some("a1"));

        assert_eq!(
            pod.metadata
                .labels
                .as_ref()
                .unwrap()
                .get("app.kubernetes.io/managed-by")
                .map(String::as_str),
            Some("scarab")
        );
    }

    #[test]
    fn build_pod_without_egress_has_no_results_volume_or_sidecar() {
        let step = step_with_attempt("run-1", "build", "a1");
        let pod = build_pod(
            "scarab-x",
            "scarab-run-1",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        let spec = pod.spec.unwrap();
        assert!(spec.volumes.is_none(), "no shared volume without egress");
        assert!(spec.init_containers.is_none(), "no sidecar without egress");
        let env = spec.containers[0].env.as_ref().unwrap();
        assert!(
            !env.iter().any(|e| e.name == "SCARAB_RESULTS"),
            "no results env"
        );
    }

    #[test]
    fn build_pod_with_egress_wires_shared_volume_and_a_native_sidecar() {
        let egress = ResultsEgress {
            base_url: "http://scarab-server".into(),
            token_secret: b"secret".to_vec(),
            sidecar_image: "ghcr.io/acme/scarab-sidecar:1".into(),
        };
        let step = step_with_attempt("run-1", "build", "a1");
        let pod = build_pod(
            "scarab-x",
            "scarab-run-1",
            &step,
            &busybox(),
            Some(&egress),
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        let spec = pod.spec.unwrap();

        // Shared results emptyDir.
        let vol = spec
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == RESULTS_VOLUME)
            .unwrap();
        assert!(vol.empty_dir.is_some());

        // Step container: mounts the volume and knows where to write.
        let stepc = spec.containers.iter().find(|c| c.name == "step").unwrap();
        assert!(stepc
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|m| m.name == RESULTS_VOLUME && m.mount_path == RESULTS_MOUNT_PATH));
        let senv = |c: &Container, k: &str| {
            c.env
                .as_ref()
                .unwrap()
                .iter()
                .find(|e| e.name == k)
                .and_then(|e| e.value.clone())
        };
        assert_eq!(
            senv(stepc, "SCARAB_RESULTS").as_deref(),
            Some(RESULTS_MOUNT_PATH)
        );

        // Native sidecar: initContainer, restartPolicy Always, fence token + URL,
        // read-only view of the results.
        let side = spec
            .init_containers
            .as_ref()
            .unwrap()
            .iter()
            .find(|c| c.name == "scarab-results-egress")
            .unwrap();
        assert_eq!(
            side.restart_policy.as_deref(),
            Some("Always"),
            "native sidecar"
        );
        assert_eq!(side.image.as_deref(), Some("ghcr.io/acme/scarab-sidecar:1"));
        assert_eq!(
            senv(side, "SCARAB_RESULTS_URL").as_deref(),
            Some("http://scarab-server/v1/runs/run-1/steps/build/results")
        );
        // Token is HMAC over the fence — matches what the server verifies.
        let expected = scarab_forge_github::sign_hex(b"secret", b"run-1:build:a1");
        assert_eq!(senv(side, "SCARAB_RESULTS_TOKEN"), Some(expected));
        assert_eq!(
            side.volume_mounts.as_ref().unwrap()[0].read_only,
            Some(true),
            "sidecar reads results read-only"
        );
    }

    /// The ADR-0061 workspace service, as a Step Pod is told about it.
    fn sample_fetch() -> WorkspaceFetch {
        WorkspaceFetch {
            url: "http://scarab-workspace".into(),
            token_secret: b"ws-secret".to_vec(),
            fetcher_image: "ghcr.io/acme/scarab-wsfetch:test".into(),
        }
    }

    fn sample_build() -> scarab_engine::BuildConfig {
        scarab_engine::BuildConfig {
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            image: "registry.example/app:1.0".into(),
            repo_owner: "acme".into(),
            repo_name: "app".into(),
            push: true,
            insecure_push: false,
            registry_auth_json: None,
            derived_auth: None,
        }
    }

    #[test]
    fn build_pod_is_rootless_buildkit_and_not_privileged() {
        let step = step_with_attempt("run-1", "image", "a1");
        let mut spec = busybox();
        spec.image = String::new();
        spec.command = vec![];
        spec.build = Some(sample_build());
        // A build step consumes its `needs` workspace like any other step, so it
        // gets the ADR-0061 fetcher too — asserted at the bottom of this test.
        spec.workspace_inputs = vec!["tree-from-clone".into()];
        let fetch = sample_fetch();
        let pod = build_pod(
            "scarab-image-a1",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
            Some(&fetch),
        );
        let c = &pod.spec.as_ref().unwrap().containers[0];

        assert_eq!(c.image.as_deref(), Some("moby/buildkit:rootless"));
        assert_eq!(
            c.command.as_ref().unwrap(),
            &vec!["buildctl-daemonless.sh".to_string()]
        );
        let args = c.args.as_ref().unwrap().join(" ");
        assert!(args.contains("--frontend dockerfile.v0"), "{args}");
        assert!(args.contains("filename=Dockerfile"), "{args}");
        assert!(
            args.contains("type=image,name=registry.example/app:1.0,push=true"),
            "{args}"
        );

        // Rootless security posture: never privileged, non-root, unconfined seccomp.
        let sc = c.security_context.as_ref().unwrap();
        assert_eq!(sc.privileged, Some(false));
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(sc.seccomp_profile.as_ref().unwrap().type_, "Unconfined");
        // AppArmor unconfined annotation for the step container.
        assert_eq!(
            pod.metadata
                .annotations
                .as_ref()
                .unwrap()
                .get(&format!(
                    "container.apparmor.security.beta.kubernetes.io/{STEP_CONTAINER}"
                ))
                .map(String::as_str),
            Some("unconfined")
        );
        // No auth resolved => no registry mount, no DOCKER_CONFIG.
        assert!(c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .all(|e| e.name != "DOCKER_CONFIG"));
        // The build consumes its `needs` workspace like any step.
        assert!(pod
            .spec
            .as_ref()
            .unwrap()
            .init_containers
            .as_ref()
            .unwrap()
            .iter()
            .any(|i| i.name == WORKSPACE_INIT_CONTAINER));
    }

    #[test]
    fn build_pod_mounts_registry_auth_and_never_puts_the_token_in_env() {
        let step = step_with_attempt("run-1", "image", "a1");
        let mut spec = busybox();
        spec.image = String::new();
        spec.command = vec![];
        let mut build = sample_build();
        build.derived_auth = Some(scarab_engine::RegistryCredential {
            registry: "registry.example".into(),
            username: "x-access-token".into(),
            token: "sekret-registry-token".into(),
        });
        spec.build = Some(build.clone());
        let pod = build_pod(
            "scarab-image-a1",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        let c = &pod.spec.as_ref().unwrap().containers[0];
        let env = c.env.as_ref().unwrap();
        assert_eq!(
            env.iter()
                .find(|e| e.name == "DOCKER_CONFIG")
                .and_then(|e| e.value.clone()),
            Some("/scarab/registry".to_string())
        );
        // THE INVARIANT (ADR-0018/0037): the token rides ONLY in the mounted
        // Secret — never in env.
        assert!(env
            .iter()
            .all(|e| e.value.as_deref() != Some("sekret-registry-token")));
        let m = c
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "scarab-registry-auth")
            .unwrap();
        assert_eq!(m.mount_path, "/scarab/registry");
        assert_eq!(m.read_only, Some(true));
        let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let v = vols
            .iter()
            .find(|v| v.name == "scarab-registry-auth")
            .unwrap();
        assert_eq!(
            v.secret.as_ref().unwrap().secret_name.as_deref(),
            Some("scarab-image-a1-registry")
        );

        // The synthesized dockerconfigjson carries the derived credential.
        let json = registry_dockerconfig(&build).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["auths"]["registry.example"]["auth"].is_string());
        // A scoped secret takes precedence, verbatim.
        build.registry_auth_json = Some("{\"auths\":{}}".into());
        assert_eq!(registry_dockerconfig(&build).unwrap(), "{\"auths\":{}}");
    }

    #[test]
    fn build_args_capture_the_digest_when_egress_is_present() {
        let step = step_with_attempt("run-1", "image", "a1");
        let mut spec = busybox();
        spec.image = String::new();
        spec.command = vec![];
        spec.build = Some(sample_build());
        let egress = ResultsEgress {
            base_url: "http://scarab:8080".into(),
            token_secret: b"k".to_vec(),
            sidecar_image: "ghcr.io/scarab/egress:1".into(),
        };
        let pod = build_pod(
            "scarab-image-a1",
            "ns",
            &step,
            &spec,
            Some(&egress),
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        let args = pod.spec.as_ref().unwrap().containers[0]
            .args
            .as_ref()
            .unwrap()
            .join(" ");
        // The digest lands as the `image` result (ADR-0041/0042) — the
        // ImageArtifact of record.
        assert!(
            args.contains("--metadata-file /scarab/results/image.json"),
            "{args}"
        );
    }

    #[test]
    fn oidc_token_is_tmpfs_mounted_never_env() {
        let step = step_with_attempt("run-1", "deploy", "a1");
        let mut spec = busybox();
        spec.oidc_token = Some("eyJhbGciOi.sekret-oidc-jwt.sig".into());
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        let c = &pod.spec.as_ref().unwrap().containers[0];
        let env = c.env.as_ref().unwrap();
        assert_eq!(
            env.iter()
                .find(|e| e.name == "SCARAB_OIDC_TOKEN_FILE")
                .and_then(|e| e.value.clone()),
            Some("/scarab/oidc/token".to_string())
        );
        // THE INVARIANT (ADR-0015): the token itself never rides in env.
        assert!(env
            .iter()
            .all(|e| e.value.as_deref() != spec.oidc_token.as_deref()));
        let m = c
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "scarab-oidc-token")
            .unwrap();
        assert_eq!(m.mount_path, "/scarab/oidc");
        assert_eq!(m.read_only, Some(true));
        let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let v = vols.iter().find(|v| v.name == "scarab-oidc-token").unwrap();
        assert_eq!(
            v.secret.as_ref().unwrap().secret_name.as_deref(),
            Some("scarab-x-oidc")
        );

        // OIDC disabled (no token) => none of the machinery appears.
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        let c = &pod.spec.as_ref().unwrap().containers[0];
        assert!(c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .all(|e| e.name != "SCARAB_OIDC_TOKEN_FILE"));
        assert!(pod.spec.as_ref().unwrap().volumes.is_none());
    }

    #[test]
    fn artifact_globs_match_segments_and_extensions() {
        assert!(glob_match("dist/*", "dist/app.tar.gz"));
        assert!(glob_match("*.tar.gz", "release.tar.gz"));
        assert!(glob_match("coverage/*.html", "coverage/index.html"));
        assert!(!glob_match("*.html", "coverage.txt"));
        assert!(glob_match("exact.txt", "exact.txt"));
        assert!(!glob_match("exact.txt", "other.txt"));
    }

    #[test]
    fn workspace_pods_get_the_artifacts_volume_and_globs_annotation() {
        let step = step_with_attempt("run-1", "build", "a1");
        let mut spec = busybox();
        spec.artifacts = vec!["dist/*".into()];
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        let c = &pod.spec.as_ref().unwrap().containers[0];
        let m = c
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "scarab-artifacts")
            .expect("artifacts mount");
        assert_eq!(m.mount_path, "/scarab/artifacts");
        assert!(c.env.as_ref().unwrap().iter().any(
            |e| e.name == "SCARAB_ARTIFACTS" && e.value.as_deref() == Some("/scarab/artifacts")
        ));
        assert_eq!(
            pod.metadata
                .annotations
                .as_ref()
                .unwrap()
                .get("scarab.io/artifact-globs")
                .map(String::as_str),
            Some(r#"["dist/*"]"#)
        );
        // The egress container (the harvest surface) also mounts it.
        let inits = pod.spec.as_ref().unwrap().init_containers.as_ref().unwrap();
        let egress = inits
            .iter()
            .find(|c| c.name == WORKSPACE_EGRESS_CONTAINER)
            .unwrap();
        assert!(egress
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|m| m.name == "scarab-artifacts"));

        // No workspace flow => no artifacts machinery.
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        assert!(pod.spec.as_ref().unwrap().volumes.is_none());
    }

    #[test]
    fn declared_outputs_ride_the_pod_annotation() {
        // ADR-0007 per-path publishing: the authored `outputs:` must live ON THE
        // POD, because the egress prune runs in `drive_workspace` — which may be
        // a *different* control plane after a restart and has no in-memory spec.
        let step = step_with_attempt("run-1", "build", "a1");
        let mut spec = busybox();
        spec.workspace_outputs = vec!["dist".into(), "reports/junit".into()];
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        assert_eq!(
            pod.metadata
                .annotations
                .as_ref()
                .unwrap()
                .get(ANNOTATION_WS_OUTPUTS)
                .map(String::as_str),
            Some("dist,reports/junit"),
            "the declared paths must be recoverable from the Pod alone"
        );

        // No `outputs:` => no annotation at all, which is what the egress leg
        // reads as "publish the whole workspace" (the implicit default).
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        assert!(
            !pod.metadata
                .annotations
                .as_ref()
                .is_some_and(|a| a.contains_key(ANNOTATION_WS_OUTPUTS)),
            "absent, not empty — an empty annotation would be an ambiguous encoding"
        );
    }

    /// git-bug 7f05f39: `ANNOTATION_WS_INPUTS` used to be inserted **even when
    /// empty**, which made one key carry two different questions — "is this a
    /// workspace Pod" (present) and "does it need a feed" (non-empty). It is now
    /// written only when there ARE inputs, and it is evidence rather than control
    /// flow: `drive_workspace` no longer reads it, and the fetcher gets its roots
    /// from its own container env.
    #[test]
    fn the_inputs_annotation_is_absent_when_there_are_no_inputs() {
        let step = step_with_attempt("run-1", "build", "a1");
        let fetch = sample_fetch();
        // A workspace Pod with no `needs:` — the shape of every `clone` step and
        // every first step in a pipeline.
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
            Some(&fetch),
        );
        assert!(
            !pod.metadata
                .annotations
                .as_ref()
                .is_some_and(|a| a.contains_key(ANNOTATION_WS_INPUTS)),
            "absent, not empty: an always-present key cannot answer two questions"
        );
        // …and it IS a workspace Pod all the same — proven by the machinery, not
        // by an annotation.
        let ps = pod.spec.as_ref().unwrap();
        assert!(ps
            .init_containers
            .as_ref()
            .unwrap()
            .iter()
            .any(|c| c.name == WORKSPACE_EGRESS_CONTAINER));
        assert_eq!(ps.security_context.as_ref().unwrap().fs_group, Some(65532));

        let mut with_inputs = busybox();
        with_inputs.workspace_inputs = vec!["tree-a".into()];
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &with_inputs,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
            Some(&fetch),
        );
        assert_eq!(
            pod.metadata.annotations.as_ref().unwrap()[ANNOTATION_WS_INPUTS],
            "tree-a"
        );
    }

    #[test]
    fn workspace_pods_trust_the_provisioned_workspace_for_git() {
        // The clone step provisions /workspace as a different uid than most
        // consuming steps; git would refuse it ("dubious ownership"). The
        // executor marks it trusted via env-based git config so no pipeline
        // author has to — present iff the workspace flow is on.
        let step = step_with_attempt("run-1", "version", "a1");
        let env_of = |workspace: bool| {
            build_pod(
                "scarab-x",
                "ns",
                &step,
                &busybox(),
                None,
                DEFAULT_STEP_TIMEOUT_SECS,
                workspace,
                DEFAULT_CLONE_IMAGE,
                None,
            )
            .spec
            .unwrap()
            .containers[0]
                .env
                .clone()
                .unwrap_or_default()
        };
        let val = |env: &[EnvVar], k: &str| {
            env.iter()
                .find(|e| e.name == k)
                .and_then(|e| e.value.clone())
        };

        let on = env_of(true);
        assert_eq!(val(&on, "GIT_CONFIG_COUNT").as_deref(), Some("1"));
        assert_eq!(
            val(&on, "GIT_CONFIG_KEY_0").as_deref(),
            Some("safe.directory")
        );
        assert_eq!(val(&on, "GIT_CONFIG_VALUE_0").as_deref(), Some("*"));

        // No workspace flow => don't inject it.
        let off = env_of(false);
        assert!(val(&off, "GIT_CONFIG_COUNT").is_none());
    }

    #[test]
    fn image_artifact_and_push_fence_record_the_digest() {
        let build = sample_build();
        let digest = "sha256:abc123";
        let artifact = image_artifact(&build, digest);
        assert_eq!(artifact.image, "registry.example/app:1.0");
        assert_eq!(artifact.digest, digest);

        // Idempotent push: the fence is keyed by content, so a re-push is one effect.
        assert_eq!(
            push_fence(&build.image, digest),
            "push:registry.example/app:1.0@sha256:abc123"
        );
    }

    #[test]
    fn pod_state_maps_phases_and_exit_code() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
        };

        let with_phase = |phase: &str, exit: Option<i32>| {
            let container_statuses = exit.map(|code| {
                vec![ContainerStatus {
                    name: "step".into(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: code,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]
            });
            Pod {
                status: Some(PodStatus {
                    phase: Some(phase.into()),
                    container_statuses,
                    ..Default::default()
                }),
                ..Default::default()
            }
        };

        assert_eq!(pod_state(&with_phase("Pending", None)), ExecState::Pending);
        assert_eq!(pod_state(&with_phase("Running", None)), ExecState::Running);
        assert_eq!(
            pod_state(&with_phase("Succeeded", Some(0))),
            ExecState::Succeeded
        );
        // A container exit code is the step's own verdict (ADR-0047).
        assert_eq!(
            pod_state(&with_phase("Failed", Some(1))),
            ExecState::Failed {
                exit_code: Some(1),
                class: FailureClass::Step,
                cause: None,
            }
        );
        assert_eq!(pod_state(&with_phase("Unknown", None)), ExecState::Lost);
        // No status yet -> not scheduled -> Pending.
        assert_eq!(pod_state(&Pod::default()), ExecState::Pending);
    }

    /// A workspace Pod, with the step container terminated at `exit`, the egress
    /// sidecar either still running or terminated, and the harvested-artifact
    /// annotation optionally recorded.
    fn settling_pod(phase: &str, exit: i32, egress_running: bool, harvested: bool) -> Pod {
        // `snapshotted: true` is the normal case for the artifact-barrier tests:
        // they are about the artifact index, and a green settle records the
        // workspace root first. The ADR-0061 part 4 tests use the raw builder to
        // say otherwise.
        settling_pod_with(phase, exit, egress_running, harvested, true)
    }

    fn settling_pod_with(
        phase: &str,
        exit: i32,
        egress_running: bool,
        harvested: bool,
        snapshotted: bool,
    ) -> Pod {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStatus,
            PodStatus,
        };
        let terminated = |name: &str, code: i32| ContainerStatus {
            name: name.into(),
            state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: code,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let egress = if egress_running {
            ContainerStatus {
                name: WORKSPACE_EGRESS_CONTAINER.into(),
                state: Some(ContainerState {
                    running: Some(ContainerStateRunning::default()),
                    ..Default::default()
                }),
                ..Default::default()
            }
        } else {
            terminated(WORKSPACE_EGRESS_CONTAINER, 0)
        };
        let mut annotations = std::collections::BTreeMap::new();
        if harvested {
            annotations.insert(
                ANNOTATION_ARTIFACTS.to_string(),
                "[{\"name\":\"dist/report.html\",\"size\":11,\
                 \"content_type\":\"text/html\",\"object_key\":\"artifacts/r/dist/report.html\"}]"
                    .to_string(),
            );
        }
        if snapshotted {
            annotations.insert(
                ANNOTATION_WS_ROOT.to_string(),
                "a".repeat(64),
            );
        }
        Pod {
            metadata: ObjectMeta {
                name: Some("scarab-x".into()),
                annotations: (!annotations.is_empty()).then_some(annotations),
                ..Default::default()
            },
            // The SPEC matters now: `is_workspace_pod` derives "is this on the
            // workspace flow?" from the egress barrier container's presence here,
            // not from an annotation (git-bug 7f05f39).
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: STEP_CONTAINER.to_string(),
                    ..Default::default()
                }],
                init_containers: Some(vec![Container {
                    name: WORKSPACE_EGRESS_CONTAINER.to_string(),
                    restart_policy: Some("Always".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status: Some(PodStatus {
                phase: Some(phase.into()),
                container_statuses: Some(vec![terminated(STEP_CONTAINER, exit)]),
                init_container_statuses: Some(vec![egress]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// 98ea804: the settle barrier. The artifact index is patched onto the Pod by
    /// the egress sidecar AFTER the step container exits, but the orchestrator
    /// indexes artifacts off the terminal verdict exactly once — so Succeeded must
    /// be withheld until that sidecar has terminated (the settle-complete signal),
    /// while every failure verdict the settle cannot inform passes through so
    /// infra faults are never masked.
    #[test]
    fn succeeded_is_withheld_until_the_settle_sidecar_has_finished() {
        // Step exited 0, settle still in flight -> NOT terminal yet.
        assert_eq!(
            settled_state(&settling_pod("Succeeded", 0, true, false), true),
            ExecState::Running,
            "reporting Succeeded here loses the artifact index permanently"
        );
        // Sidecar gone = settle recorded -> the real verdict.
        assert_eq!(
            settled_state(&settling_pod("Succeeded", 0, false, true), true),
            ExecState::Succeeded
        );
    }

    // ------------------------------------------------------------------
    // ADR-0064 control-plane half (ticket 4cf03d7): a failed Depot drain is
    // Transient — bounded by TIME — and then EvidenceLost. Pure over
    // (fatal, first_failure_ms, now_ms): the interleavings are constructed,
    // never scheduled (this repo's concurrency-test lesson).
    // ------------------------------------------------------------------

    /// A `FlushOutcome::Fatal` escalates IMMEDIATELY, first failure or not:
    /// the Depot has said this snapshot can never become durable, so one more
    /// transient lap would only push the truth toward the step-budget timeout.
    /// Kills the mutation that drops the `if fatal` short-circuit (Fatal would
    /// then present as a retriable blip until the 5-minute clock ran out).
    #[test]
    fn a_fatal_flush_is_evidence_lost_immediately() {
        // No prior failure recorded — the very first observation.
        assert_eq!(
            drain_failure_verdict(true, None, 1_000),
            DrainVerdict::EvidenceLost
        );
        // And equally with a fresh clock already running.
        assert_eq!(
            drain_failure_verdict(true, Some(999), 1_000),
            DrainVerdict::EvidenceLost
        );
    }

    /// A transient failure inside the 5-minute window keeps re-driving — both
    /// on the FIRST failure (no anchor yet: the caller records it) and while
    /// the anchored clock is still young. Kills the inverted-default mutation
    /// (escalating on the first blip would fail an Attempt on every Depot
    /// hiccup and every routine rollout).
    #[test]
    fn a_transient_drain_failure_within_the_window_re_drives() {
        let now = 10 * 60 * 1000;
        assert_eq!(drain_failure_verdict(false, None, now), DrainVerdict::Transient);
        // Exactly at the bound is still transient ("past 5 minutes", not "at").
        assert_eq!(
            drain_failure_verdict(false, Some(now - WS_DRAIN_ESCALATION_MS), now),
            DrainVerdict::Transient
        );
        assert_eq!(
            drain_failure_verdict(false, Some(now - WS_DRAIN_ESCALATION_MS + 1), now),
            DrainVerdict::Transient
        );
    }

    /// Past the window the verdict is EvidenceLost (the caller carries the
    /// LAST cause). Kills the mutation that removes the time bound (a Depot
    /// outage would then grind transiently until the step budget expired —
    /// exactly the illegible timeout 4cf03d7 forbids) and the `<`/`>` flip.
    #[test]
    fn a_transient_drain_failure_past_the_window_is_evidence_lost() {
        let now = 60 * 60 * 1000;
        assert_eq!(
            drain_failure_verdict(false, Some(now - WS_DRAIN_ESCALATION_MS - 1), now),
            DrainVerdict::EvidenceLost
        );
        // A clock skewed into the future must not underflow into escalation.
        assert_eq!(
            drain_failure_verdict(false, Some(now + 1_000), now),
            DrainVerdict::Transient
        );
    }

    // ------------------------------------------------------------------
    // Ticket 66c93be: anchor SELECTION. When the durable annotation anchor
    // can be neither read nor written on EVERY poll (revoked patch RBAC, a
    // broken admission webhook), `drain_failure_verdict` must not be starved
    // of an anchor forever — the in-process fallback map supplies one.
    // Pure over (annotation_result, map): the outage is constructed, never
    // scheduled.
    // ------------------------------------------------------------------

    /// (1) The annotation anchor is usable → it is authoritative: used
    /// verbatim, never seeded into or promoted from the fallback map — a
    /// stale in-memory entry must not be trusted over the durable clock, and
    /// a healthy path must not grow the map. The stray PRUNE still runs on
    /// this path (it runs on every invocation): a stray seeded during an RBAC
    /// outage must not survive the process lifetime just because the
    /// annotation path handles every failure after the heal. Kills the
    /// mutation that consults the fallback unconditionally (a pre-RBAC-fix
    /// stale entry would then escalate a healthy drain) and the one that
    /// prunes only on a fallback insert.
    #[test]
    fn a_usable_annotation_anchor_wins_and_is_never_seeded_into_the_fallback_map() {
        let mut map = std::collections::HashMap::new();
        map.insert("pod-a".to_string(), 111_i64);
        let now = 100 * 60 * 1000;
        // A stray from a healed outage, older than 2x the window.
        map.insert(
            "pod-stray-stale".to_string(),
            now - 2 * WS_DRAIN_ESCALATION_MS - 1,
        );
        let (first, anchor_err) =
            select_drain_anchor(Ok(5_000), &mut map, "pod-a", now);
        assert_eq!(first, 5_000, "the durable annotation anchor is authoritative");
        assert_eq!(anchor_err, None, "no anchor failure to report");
        assert_eq!(
            map.get("pod-a"),
            Some(&111),
            "the current key's entry is neither promoted nor re-seeded"
        );
        assert!(
            !map.contains_key("pod-stray-stale"),
            "the stray prune runs on the annotation path too — after RBAC heals, \
             no fallback insert ever happens again to trigger it"
        );
    }

    /// (2) Annotation unwritable, map empty → the fallback seeds itself with
    /// `now` and the verdict is Transient (the outage clock STARTS, it does
    /// not fire). Kills the mutation that drops the whole fallback (the
    /// verdict would still be Transient here, but nothing would be seeded —
    /// which is what leaves case (3) Transient forever).
    #[test]
    fn an_unwritable_anchor_seeds_the_fallback_clock_and_stays_transient() {
        let mut map = std::collections::HashMap::new();
        let now = 10 * 60 * 1000;
        let (first, anchor_err) =
            select_drain_anchor(Err("patch denied".into()), &mut map, "pod-a", now);
        assert_eq!(first, now, "first fallback observation anchors at now");
        assert_eq!(anchor_err.as_deref(), Some("patch denied"));
        assert_eq!(map.get("pod-a"), Some(&now), "the clock is now seeded");
        assert_eq!(
            drain_failure_verdict(false, Some(first), now),
            DrainVerdict::Transient,
            "the first observed failure never escalates"
        );
    }

    /// (3) Annotation unwritable on every poll, fallback entry older than the
    /// window → EvidenceLost, and the cause names BOTH failures (the drain's
    /// own AND the anchor write's). Kills two mutations by name:
    /// - drop the fallback entirely → `first_failure_ms` is None on every
    ///   poll and this case stays Transient forever (the step-budget-timeout
    ///   disguise 4cf03d7 forbids);
    /// - drop insert-if-absent (always overwrite with `now`) → the clock
    ///   restarts on every poll and never ages past the window, so this case
    ///   ALSO never escalates.
    #[test]
    fn a_fallback_clock_past_the_window_escalates_naming_both_failures() {
        let mut map = std::collections::HashMap::new();
        let seeded = 10 * 60 * 1000_i64;
        map.insert("pod-a".to_string(), seeded);
        let now = seeded + WS_DRAIN_ESCALATION_MS + 1;
        let (first, anchor_err) =
            select_drain_anchor(Err("patch denied".into()), &mut map, "pod-a", now);
        assert_eq!(
            first, seeded,
            "insert-if-absent: the ORIGINAL fallback clock is observed, never restarted"
        );
        assert_eq!(
            drain_failure_verdict(false, Some(first), now),
            DrainVerdict::EvidenceLost,
            "past the window the fallback clock escalates"
        );
        let cause = evidence_lost_cause("flush: depot unreachable".to_string(), anchor_err);
        assert!(
            cause.contains("flush: depot unreachable"),
            "the drain failure itself is named: {cause}"
        );
        assert!(
            cause.contains("failure anchor could not be recorded on the Pod")
                && cause.contains("patch denied"),
            "the anchor-write failure is named too: {cause}"
        );
    }

    /// The unbounded-growth cap: a fallback insert prunes OTHER entries older
    /// than 2× the window (strays from paths that reached neither success nor
    /// escalation), but never the current key — its clock may legally be old
    /// at the moment it is being read. Kills the mutation that prunes before
    /// the insert-if-absent read (the current key's old clock would be
    /// dropped and re-seeded, restarting it).
    #[test]
    fn a_fallback_insert_prunes_stale_strays_but_never_the_current_key() {
        let mut map = std::collections::HashMap::new();
        let now = 100 * 60 * 1000_i64;
        let stale = now - 2 * WS_DRAIN_ESCALATION_MS - 1;
        let fresh = now - WS_DRAIN_ESCALATION_MS;
        map.insert("pod-stray-stale".to_string(), stale);
        map.insert("pod-stray-fresh".to_string(), fresh);
        map.insert("pod-a".to_string(), stale);
        let (first, _) =
            select_drain_anchor(Err("patch denied".into()), &mut map, "pod-a", now);
        assert_eq!(first, stale, "the current key survives the prune, however old");
        assert!(
            !map.contains_key("pod-stray-stale"),
            "a stray older than 2x the window is pruned"
        );
        assert_eq!(
            map.get("pod-stray-fresh"),
            Some(&fresh),
            "a stray still inside 2x the window is kept"
        );
    }

    // ------------------------------------------------------------------
    // ADR-0064 s2: the durability stamp. Pure over the flush outcome — the
    // only annotation-read coverage in this crate is live-tier, so the value
    // DERIVATION is the unit under test, not the Pod round-trip.
    // ------------------------------------------------------------------

    /// A named cold tier is stamped verbatim. Kills the mutation that hardcodes
    /// the stamp (e.g. always `"object"`): a `separate-volume` deployment would
    /// then audit as object-store-durable — a durability claim about the wrong
    /// medium.
    #[test]
    fn a_durable_flush_stamps_the_tier_the_depot_named() {
        use scarab_workspace_client::FlushOutcome;
        assert_eq!(
            durability_stamp(&FlushOutcome::Durable {
                tier: Some("object".into())
            }),
            Some("object")
        );
        assert_eq!(
            durability_stamp(&FlushOutcome::Durable {
                tier: Some("separate-volume".into())
            }),
            Some("separate-volume")
        );
    }

    /// `WarmOnly` stamps `"warm-only"`. Kills the mutation that maps WarmOnly
    /// to `None`: every warm-only deployment would then silently un-stamp —
    /// indistinguishable from pre-s2 Pods, and the "this evidence is one disk
    /// away from gone" audit signal ADR-0064 s2 exists for would never appear.
    #[test]
    fn a_warm_only_flush_stamps_warm_only() {
        use scarab_workspace_client::FlushOutcome;
        assert_eq!(
            durability_stamp(&FlushOutcome::WarmOnly),
            Some("warm-only")
        );
    }

    /// `Durable { tier: None }` — an old Depot affirming durability without
    /// naming a tier (rolling-upgrade skew) — stamps NOTHING. Kills the
    /// mutation that defaults the missing tier (e.g. to `"object"` or
    /// `"warm-only"`): an absent stamp is honest, a guessed one is a fabricated
    /// durability claim.
    ///
    /// `Retry`/`Fatal` are deliberately untested here: they are unreachable
    /// from today's one caller — the flush match returns out through
    /// `drain_failure` before any patch, and only the surviving arms call
    /// `durability_stamp`. (If a future caller ever slips one through, the
    /// production posture is `None` + debug_assert, not a panic — a wrong
    /// stamp-skip beats taking the poll path down.)
    #[test]
    fn a_durable_flush_without_a_tier_stamps_nothing() {
        use scarab_workspace_client::FlushOutcome;
        assert_eq!(durability_stamp(&FlushOutcome::Durable { tier: None }), None);
    }

    // ------------------------------------------------------------------
    // ADR-0061 part 4 (s4): an Attempt is not `Succeeded` until its Workspace
    // Snapshot is durable.
    // ------------------------------------------------------------------

    /// The load-bearing case, and the one ADR-0061 part 4 is written about: on spot
    /// a node vanishes between "the Step exited 0" and "its evidence is safe".
    ///
    /// The pre-existing withholding rule reasons from a **proxy** — the egress
    /// sidecar is still running, therefore the settle is in flight — and that proxy
    /// is only sound for the way `drive_workspace` *releases* the sidecar. A node
    /// that disappears, or a 600 s termination grace that expires and SIGKILLs a
    /// sidecar deliberately ignoring SIGTERM, terminates it with no root recorded.
    /// The proxy then says "settled" while the product is absent, and `Succeeded`
    /// would be a claim the durable record cannot back: the step goes green with no
    /// evidence, and the engine will not catch it (on `Succeeded` it records
    /// `output()` *if `Some`* and finalises either way).
    #[test]
    fn succeeded_is_refused_when_the_snapshot_was_lost_with_the_pod() {
        // Step exited 0, sidecar gone, NO workspace root ⇒ the snapshot is gone.
        let lost = settling_pod_with("Succeeded", 0, false, true, false);
        assert!(workspace_snapshot_lost(&lost));
        match settled_state(&lost, true) {
            ExecState::Failed {
                exit_code: None,
                // The process RAN, so a side effect is possible: the retry is the
                // at-least-once kind (CONTEXT.md §2), not "safe, it never started".
                class:
                    FailureClass::Infra {
                        never_started: false,
                    },
                cause: Some(cause),
            } => assert!(
                cause.contains("workspace snapshot lost"),
                "the verdict must carry a legible cause (4cf03d7), got: {cause}"
            ),
            other => panic!(
                "green with no evidence is the one verdict this product may not \
                 issue — got {other:?}"
            ),
        }

        // The same Pod WITH the root recorded is the ordinary green path.
        let settled = settling_pod_with("Succeeded", 0, false, true, true);
        assert!(!workspace_snapshot_lost(&settled));
        assert_eq!(settled_state(&settled, true), ExecState::Succeeded);
    }

    /// It must not become a way to hang a Run either. While the sidecar is alive
    /// the FIRST rule still applies — withhold as `Running`, because the drain is
    /// genuinely in flight and the next poll drives it — and the failure verdict is
    /// reached only once the sidecar is gone, i.e. once the snapshot provably
    /// cannot appear. Withholding is bounded by `activeDeadlineSeconds` on the Pod
    /// and by the engine's timeout backstop; this rule is bounded by the sidecar's
    /// life.
    #[test]
    fn a_drain_still_in_flight_is_withheld_not_failed() {
        let draining = settling_pod_with("Succeeded", 0, true, false, false);
        assert_eq!(
            settled_state(&draining, true),
            ExecState::Running,
            "the drain has not run yet — this is not a lost snapshot, it is a pending one"
        );
    }

    /// A **failed** step owes no snapshot: `drive_workspace` snapshots successful
    /// steps only, because a failed step's workspace is not an output. Its
    /// artifacts ARE owed, and that is `artifact_harvest_owed`'s rule — this one
    /// must not reclassify a step's own failure as infra, which would burn the
    /// infra retry budget and mislabel a developer's broken build as an operator
    /// problem.
    #[test]
    fn a_failed_step_owes_no_snapshot() {
        let failed = settling_pod_with("Failed", 1, false, true, false);
        assert!(!workspace_snapshot_lost(&failed));
        assert_eq!(
            settled_state(&failed, true),
            ExecState::Failed {
                exit_code: Some(1),
                class: FailureClass::Step,
                cause: None,
            }
        );
    }

    /// A non-workspace Pod (no CAS wired when it was built, so no egress barrier in
    /// its spec) has no snapshot to owe and must pass through untouched. Derived
    /// from the SPEC rather than an annotation — a control-plane restart has no
    /// in-memory state, and an annotation whose presence means one thing and whose
    /// emptiness means another is the conflation git-bug 7f05f39 was about.
    #[test]
    fn a_non_workspace_pod_owes_no_snapshot() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
        };
        let pod = Pod {
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: STEP_CONTAINER.to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            status: Some(PodStatus {
                phase: Some("Succeeded".into()),
                container_statuses: Some(vec![ContainerStatus {
                    name: STEP_CONTAINER.into(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: 0,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!is_workspace_pod(&pod));
        assert!(!workspace_snapshot_lost(&pod));
        assert_eq!(settled_state(&pod, false), ExecState::Succeeded);
    }

    /// An EMPTY workspace still has a root — the empty tree's hash — so a step that
    /// produced nothing is `Succeeded`, not "snapshot lost". The distinction is
    /// between *no annotation* and *an annotation naming an empty tree*, and only
    /// the first is a lost snapshot.
    #[test]
    fn an_empty_but_recorded_snapshot_is_not_a_lost_one() {
        let mut pod = settling_pod_with("Succeeded", 0, false, true, false);
        pod.metadata.annotations.as_mut().unwrap().insert(
            ANNOTATION_WS_ROOT.to_string(),
            // Whatever `ingest` returns for an empty directory: still a root.
            "e".repeat(64),
        );
        assert!(!workspace_snapshot_lost(&pod));
        assert_eq!(settled_state(&pod, true), ExecState::Succeeded);

        // …whereas an EMPTY-STRING annotation is not a root and must not pass. The
        // annotation is only ever written from a real `Snapshot.root`, so this is
        // belt-and-braces against a future writer that "clears" it.
        pod.metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert(ANNOTATION_WS_ROOT.to_string(), String::new());
        assert!(workspace_snapshot_lost(&pod));
    }

    /// A platform verdict about a green-looking Pod must not be reclassified. If
    /// the Pod reports `Failed`/`DeadlineExceeded`, that is the verdict — the
    /// snapshot rule only ever intercepts `Succeeded`, so a wedged drain that runs
    /// the Pod past its `activeDeadlineSeconds` surfaces as `Timeout` (which is
    /// bounded and honest) rather than as this rule's infra failure.
    #[test]
    fn the_snapshot_rule_only_intercepts_succeeded() {
        let mut pod = settling_pod_with("Failed", 137, false, true, false);
        pod.status.as_mut().unwrap().reason = Some("DeadlineExceeded".into());
        assert_eq!(
            settled_state(&pod, true),
            ExecState::Failed {
                exit_code: Some(137),
                class: FailureClass::Timeout,
                cause: None,
            },
            "a wedged drain that outlives the step deadline is a Timeout, not a lost \
             snapshot — and never a Succeeded"
        );
    }

    /// a28a173: the same barrier now covers the step's OWN failure verdict —
    /// otherwise the failed attempt's artifacts (the JUnit XML, the crash log) are
    /// uploaded and never indexed, because the scheduler reads `artifacts()` off
    /// the terminal verdict exactly once. It is withheld only while the harvest is
    /// genuinely owed, so a wedged sidecar can never hide a verdict we already
    /// know — and the class is always the step's own, never the harvest's.
    #[test]
    fn a_step_failure_is_withheld_only_while_its_artifact_harvest_is_owed() {
        let failed = ExecState::Failed {
            exit_code: Some(1),
            class: FailureClass::Step,
            cause: None,
        };
        // Step exited 1, index not yet on the Pod -> withhold, or the evidence
        // is uploaded-but-unindexed forever.
        assert_eq!(
            settled_state(&settling_pod("Failed", 1, true, false), true),
            ExecState::Running,
            "reporting Failed here loses the failed attempt's artifact index"
        );
        // Index recorded -> report the failure at once; do not wait on the drain.
        assert_eq!(
            settled_state(&settling_pod("Failed", 1, true, true), true),
            failed,
            "the harvest landed — a still-draining sidecar must not hide the verdict"
        );
        assert_eq!(
            settled_state(&settling_pod("Failed", 1, false, true), true),
            failed
        );
        // No artifact store wired -> nothing is ever owed, nothing is withheld.
        assert_eq!(
            settled_state(&settling_pod("Failed", 1, true, false), false),
            failed
        );
    }

    /// a28a173: only the step's own verdict participates in the harvest barrier.
    /// An infra/timeout/config failure is the platform's verdict about a step that
    /// may never have produced anything, and masking it as Running would burn the
    /// step's timeout and re-classify the failure. It passes through verbatim even
    /// with the sidecar running and the harvest owed.
    #[test]
    fn a_platform_failure_is_never_masked_by_the_harvest_barrier() {
        let mut pod = settling_pod("Failed", 137, true, false);
        pod.status.as_mut().unwrap().reason = Some("DeadlineExceeded".into());
        assert!(
            artifact_harvest_owed(&pod, true),
            "precondition: the harvest is outstanding"
        );
        assert_eq!(
            settled_state(&pod, true),
            ExecState::Failed {
                exit_code: Some(137),
                class: FailureClass::Timeout,
                cause: None,
            },
            "a timeout must not be withheld as Running"
        );
    }

    /// 98ea804 + a28a173: the harvest is owed until its index is durably ON the
    /// Pod — which is what lets a failed harvest be retried (transiently, holding
    /// the barrier) instead of releasing the sidecar with the blobs uploaded and
    /// nothing indexed, and what makes a completed harvest once-only across
    /// re-polls. It is owed for EVERY terminated step, not just the green ones.
    #[test]
    fn the_artifact_harvest_is_owed_until_its_index_is_recorded() {
        assert!(
            artifact_harvest_owed(&settling_pod("Succeeded", 0, true, false), true),
            "no index recorded yet — the barrier still owes a harvest"
        );
        assert!(
            !artifact_harvest_owed(&settling_pod("Succeeded", 0, true, true), true),
            "index recorded — a re-poll must not re-harvest (once-only)"
        );
        assert!(
            artifact_harvest_owed(&settling_pod("Failed", 1, true, false), true),
            "a failed step's artifacts are evidence — often THE evidence (a28a173)"
        );
        assert!(
            !artifact_harvest_owed(&settling_pod("Failed", 1, true, true), true),
            "index recorded — a failed step's harvest is once-only too"
        );
        assert!(
            !artifact_harvest_owed(&settling_pod("Running", 0, true, false), false),
            "no artifact store wired — nothing is ever owed"
        );
    }

    #[test]
    fn build_pod_workspace_machinery_shape() {
        let step = step_with_attempt("run-1", "build", "a1");
        let mut spec = busybox();
        spec.workspace_inputs = vec!["tree-a".into(), "tree-b".into()];
        let fetch = sample_fetch();
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
            Some(&fetch),
        );

        // The input roots ride on the Pod as evidence of what it was fed.
        assert_eq!(
            pod.metadata.annotations.as_ref().unwrap()["scarab.io/workspace-inputs"],
            "tree-a,tree-b"
        );
        let ps = pod.spec.as_ref().unwrap();
        // /workspace + the control handshake dir are emptyDirs.
        let vols: Vec<_> = ps
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .map(|v| v.name.as_str())
            .collect();
        assert!(
            vols.contains(&"scarab-workspace") && vols.contains(&"scarab-ctl"),
            "{vols:?}"
        );
        // The init container FETCHES (ADR-0061 s3-feed) — no marker, no wait loop.
        // The egress sidecar (restartPolicy Always) still holds the Pod for the
        // drain, which s3-drain owns (git-bug 7f05f39).
        let inits = ps.init_containers.as_ref().unwrap();
        let init = inits
            .iter()
            .find(|c| c.name == WORKSPACE_INIT_CONTAINER)
            .unwrap();
        assert!(
            init.command.is_none(),
            "the fetcher image's entrypoint IS the fetcher; there is nothing to script"
        );
        let egress = inits
            .iter()
            .find(|c| c.name == WORKSPACE_EGRESS_CONTAINER)
            .unwrap();
        assert_eq!(egress.restart_policy.as_deref(), Some("Always"));
        assert!(egress.command.as_ref().unwrap()[2].contains("egress-done"));
        // Nothing anywhere in the Pod still waits on the deleted feed marker.
        let rendered = serde_json::to_string(&pod).unwrap();
        assert!(
            !rendered.contains("init-done"),
            "the init-done marker and its wait loop are deleted, not merely unused"
        );
        // The step runs IN the workspace, which is writable via the workspace group.
        assert_eq!(ps.containers[0].working_dir.as_deref(), Some("/workspace"));
        assert_eq!(ps.security_context.as_ref().unwrap().fs_group, Some(65532));
        assert_eq!(ps.termination_grace_period_seconds, Some(600));

        // No inputs => NO init container at all. The busybox doorstop that used to
        // run `exit 0` here existed only to be a barrier, and there is no barrier.
        let mut spec = busybox();
        spec.workspace_inputs = vec![];
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
            Some(&fetch),
        );
        let inits = pod.spec.as_ref().unwrap().init_containers.clone().unwrap();
        assert!(
            !inits.iter().any(|c| c.name == WORKSPACE_INIT_CONTAINER),
            "a step with nothing to fetch must not pay for a container"
        );

        // workspace=false => none of the machinery appears (unchanged shape).
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        assert!(pod.metadata.annotations.is_none());
        assert!(pod.spec.as_ref().unwrap().init_containers.is_none());
    }

    /// e10cf7e: the release command and the sidecar's wait-loop must name the
    /// SAME marker file. The release is now issued from three places through
    /// one helper (`release_egress_sidecar`): drive_workspace's settle, and
    /// poll's terminal OutputContract / EvidenceLost arms — which previously
    /// bailed without releasing and stranded the SIGTERM-proof sidecar (Pod
    /// phase-Running forever). The exec itself needs a cluster (the live tier
    /// exercises it); what IS assertable in-process is the mutation that would
    /// silently reintroduce the strand: the touch and the `until [ -f … ]`
    /// drifting onto different paths.
    #[test]
    fn egress_release_touches_the_marker_the_sidecar_waits_on() {
        let cmd = egress_release_cmd();
        let marker = egress_done_marker();
        assert_eq!(cmd, format!("touch {marker}"), "release is a bare touch");
        assert!(
            marker.starts_with(CTL_MOUNT_PATH),
            "the marker must live on the control handshake mount the exec \
             container actually has: {marker}"
        );
        // The rendered sidecar waits on that exact path (not merely any
        // string containing "egress-done").
        let step = step_with_attempt("run-1", "build", "a1");
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        let inits = pod.spec.as_ref().unwrap().init_containers.clone().unwrap();
        let egress = inits
            .iter()
            .find(|c| c.name == WORKSPACE_EGRESS_CONTAINER)
            .expect("workspace pods carry the egress sidecar");
        let wait = &egress.command.as_ref().unwrap()[2];
        assert!(
            wait.contains(&format!("[ -f {marker} ]")),
            "sidecar wait-loop must poll the file the release touches; \
             got: {wait}"
        );
    }

    /// ADR-0061 s3-feed acceptance, at the grain `build_pod` owns: the feed
    /// container is the Scarab-owned fetcher, it is told the service URL and the
    /// roots **in merge order**, the token reaches it on tmpfs and NOWHERE else,
    /// and it runs non-root under the ADR-0039 baseline.
    #[test]
    fn the_feed_container_is_the_scarab_fetcher_with_a_tmpfs_token() {
        let step = step_with_attempt("run-1", "build", "a1");
        let mut spec = busybox();
        // Deliberately NOT sorted: a later input overlays an earlier one
        // (ADR-0007), so the env must preserve the authored order.
        spec.workspace_inputs = vec!["tree-b".into(), "tree-a".into()];
        let fetch = sample_fetch();
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
            Some(&fetch),
        );
        let ps = pod.spec.as_ref().unwrap();
        let init = ps
            .init_containers
            .as_ref()
            .unwrap()
            .iter()
            .find(|c| c.name == WORKSPACE_INIT_CONTAINER)
            .expect("the fetcher init container");

        assert_eq!(
            init.image.as_deref(),
            Some("ghcr.io/acme/scarab-wsfetch:test"),
            "Scarab-owned image, never a busybox doorstop"
        );
        let env = init.env.as_ref().unwrap();
        let get = |k: &str| {
            env.iter()
                .find(|e| e.name == k)
                .and_then(|e| e.value.clone())
        };
        assert_eq!(
            get("SCARAB_WORKSPACE_URL").as_deref(),
            Some("http://scarab-workspace")
        );
        assert_eq!(
            get("SCARAB_SNAPSHOT_ROOTS").as_deref(),
            Some("tree-b,tree-a"),
            "merge order is the AUTHORED order — sorting here would change the result"
        );
        assert_eq!(
            get("SCARAB_WORKSPACE_TOKEN_FILE").as_deref(),
            Some("/scarab/secrets/workspace-token")
        );
        assert_eq!(get("SCARAB_WORKSPACE_TARGET").as_deref(), Some("/workspace"));

        // THE INVARIANT (ADR-0061 D1.4): the token itself is nowhere in the Pod
        // spec — not in the fetcher's env, not in the step's, not in an annotation.
        // Only a Secret *reference* is, and the Secret is tmpfs at mount time.
        let minted = workspace_token::mint(
            &fetch.token_secret,
            &workspace_token::step_claims(
                workspace_token::Fence {
                    run: "run-1".into(),
                    step: "build".into(),
                    attempt: "a1".into(),
                },
                0,
                spec.workspace_inputs.clone(),
            ),
        );
        let signature = minted.rsplit('.').next().unwrap().to_string();
        let rendered = serde_json::to_string(&pod).unwrap();
        assert!(
            !rendered.contains(&signature),
            "a workspace token must never appear in a PodSpec — it is readable by \
             anyone with `get pod`"
        );

        // tmpfs Secret volume, read-only, on the FETCHER only.
        let mount = init
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == WORKSPACE_TOKEN_VOLUME)
            .expect("token mount");
        assert_eq!(mount.mount_path, "/scarab/secrets");
        assert_eq!(mount.read_only, Some(true));
        let volume = ps
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == WORKSPACE_TOKEN_VOLUME)
            .expect("token volume");
        assert_eq!(
            volume.secret.as_ref().unwrap().secret_name.as_deref(),
            Some("scarab-x-workspace")
        );
        assert!(
            !ps.containers[0]
                .volume_mounts
                .as_ref()
                .unwrap()
                .iter()
                .any(|m| m.name == WORKSPACE_TOKEN_VOLUME),
            "the untrusted step container must not be able to read the token"
        );

        // ADR-0039: the helper is non-root at uid 65532 with every capability
        // dropped. `busybox:latest` is refused by this baseline, which is exactly
        // why the doorstop had to be pinned — a Scarab-owned image just complies.
        let sc = init.security_context.as_ref().expect("baseline");
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(sc.run_as_user, Some(65532));
        assert_eq!(sc.privileged, Some(false));
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        assert_eq!(
            sc.capabilities.as_ref().unwrap().drop,
            Some(vec!["ALL".to_string()])
        );
        assert_eq!(sc.seccomp_profile.as_ref().unwrap().type_, "RuntimeDefault");
    }

    /// Guard #1 of ADR-0061 D2.3, control-plane half: every eagerly-provisioned
    /// Pod bumps a counter, so "are we still standing on the stepping stone?" is
    /// an observable number rather than a code read.
    #[test]
    fn every_eager_feed_is_counted() {
        let step = step_with_attempt("run-1", "build", "a1");
        let mut spec = busybox();
        spec.workspace_inputs = vec!["tree-a".into()];
        let fetch = sample_fetch();
        let before = eager_fetch_total();
        for _ in 0..2 {
            build_pod(
                "scarab-x",
                "ns",
                &step,
                &spec,
                None,
                DEFAULT_STEP_TIMEOUT_SECS,
                true,
                DEFAULT_CLONE_IMAGE,
                Some(&fetch),
            );
        }
        assert_eq!(eager_fetch_total(), before + 2);
        // A Pod with nothing to fetch is not an eager fetch.
        let at = eager_fetch_total();
        build_pod(
            "scarab-x",
            "ns",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
            Some(&fetch),
        );
        assert_eq!(eager_fetch_total(), at);
    }

    /// Fail-closed, and the reason it must be: a Step whose `/workspace` is
    /// silently empty does not fail — it produces a WRONG ANSWER, and then claims
    /// in the durable record to have tested a tree that was never there. There is
    /// deliberately no fallback to the deleted control-plane feed (ADR-0061 D2.3).
    #[test]
    fn a_step_with_inputs_and_no_workspace_service_cannot_be_launched() {
        let mut spec = busybox();
        spec.workspace_inputs = vec!["tree-a".into()];
        let err = workspace_feed_is_satisfiable(&spec, true, None).unwrap_err();
        assert!(err.contains("no workspace service is configured"), "{err}");

        // A service configured ⇒ fine.
        let fetch = sample_fetch();
        assert!(workspace_feed_is_satisfiable(&spec, true, Some(&fetch)).is_ok());
        // No inputs ⇒ nothing to fetch ⇒ no service needed (every `clone` step).
        assert!(workspace_feed_is_satisfiable(&busybox(), true, None).is_ok());
        // Workspace flow off entirely (tests / object-store-less dev) ⇒ untouched.
        assert!(workspace_feed_is_satisfiable(&spec, false, None).is_ok());
    }

    /// The fetcher can FAIL where the doorstop it replaced structurally could not,
    /// so a Pod whose workspace was never provisioned must get a verdict instead
    /// of sitting `Pending` until the step timeout. It is
    /// `Infra { never_started: true }` — the same verdict the deleted
    /// `DriveErr::InputMissing` produced, because the same thing is true: the
    /// step's process never ran, so no side effect is possible.
    #[test]
    fn a_failed_workspace_fetch_is_never_started_infra_not_a_hang() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStateWaiting, ContainerStatus,
            PodStatus,
        };
        let fetch_exited = |code: i32| ContainerStatus {
            name: WORKSPACE_INIT_CONTAINER.into(),
            state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: code,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        // The step container exists but never ran — it is `waiting: PodInitializing`.
        let step_waiting = ContainerStatus {
            name: STEP_CONTAINER.into(),
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("PodInitializing".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pod = |phase: &str, code: i32| Pod {
            status: Some(PodStatus {
                phase: Some(phase.into()),
                container_statuses: Some(vec![step_waiting.clone()]),
                init_container_statuses: Some(vec![fetch_exited(code)]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let never_started = |fetch_exit: i32| ExecState::Failed {
            exit_code: None,
            class: FailureClass::Infra {
                never_started: true,
            },
            // The cause names the failed fetch and its exit (4cf03d7).
            cause: Some(format!(
                "workspace fetch init container failed (exit {fetch_exit}) — \
                 the step's inputs were never provisioned (ADR-0061 s3-feed)"
            )),
        };
        // Transient (1) and permanent (2) both land here: the class is about
        // whether a side effect was possible, not about why the fetch failed.
        assert_eq!(pod_state(&pod("Failed", 1)), never_started(1));
        assert_eq!(pod_state(&pod("Failed", 2)), never_started(2));
        // And before the phase flips: an init container that exited non-zero under
        // `restartPolicy: Never` is never retried, so there is nothing to wait for.
        assert_eq!(pod_state(&pod("Pending", 1)), never_started(1));
        // A fetcher that SUCCEEDED is not a failure signal.
        assert_eq!(pod_state(&pod("Pending", 0)), ExecState::Pending);
    }

    /// The narrowness of `workspace_fetch_failed` is load-bearing: the egress
    /// barrier and the results sidecar are ALSO `init_container_statuses` entries,
    /// they are terminated by the kubelet when the Pod ends, and they routinely
    /// exit non-zero (137) for a step that ran and produced a real verdict.
    /// Reading them as provisioning failures would report `never_started: true`
    /// for a step that had side effects — the one thing that classification exists
    /// to get right (ADR-0047).
    #[test]
    fn a_dying_sidecar_is_not_a_failed_workspace_fetch() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
        };
        let killed = |name: &str| ContainerStatus {
            name: name.into(),
            state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: 137,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pod = Pod {
            status: Some(PodStatus {
                phase: Some("Failed".into()),
                container_statuses: Some(vec![ContainerStatus {
                    name: STEP_CONTAINER.into(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: 1,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                init_container_statuses: Some(vec![
                    killed(WORKSPACE_EGRESS_CONTAINER),
                    killed("scarab-results-egress"),
                    killed("service-0"),
                    // The fetcher itself SUCCEEDED.
                    ContainerStatus {
                        name: WORKSPACE_INIT_CONTAINER.into(),
                        state: Some(ContainerState {
                            terminated: Some(ContainerStateTerminated {
                                exit_code: 0,
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(workspace_fetch_failed(&pod).is_none());
        assert_eq!(
            pod_state(&pod),
            ExecState::Failed {
                exit_code: Some(1),
                class: FailureClass::Step,
                cause: None,
            },
            "the step's own verdict, not a sidecar's exit code"
        );
    }

    // git-bug b04697f (dogfood): "clone, then build IN the workspace" — the single
    // most common CI shape — must be able to WRITE the CAS-restored `/workspace`.
    //
    // The workspace arrives owned by uid/gid 65532 — the FETCHER's uid now
    // (ADR-0061 s3-feed), the control plane's before that — and is made
    // group-writable at feed time, while the ADR-0039 baseline drops ALL
    // capabilities — `DAC_OVERRIDE` included — so *group membership* is the whole
    // mechanism, even for an admitted `run_as_root` step at uid 0. Assert that
    // membership is granted explicitly on every workspace Pod, and that it holds
    // when a uid-pinning sidecar service (ADR-0058) takes the Pod's `fsGroup`.
    //
    // The widening itself (`chmod -R g+rwX` + setgid dirs) moved INTO the fetcher
    // and is tested there:
    // `crates/scarab-workspace-client/src/bin/scarab-wsfetch.rs::widening_grants_the_group_exactly_what_a_step_needs`.
    #[test]
    fn workspace_steps_are_always_members_of_the_workspace_group() {
        use scarab_pipeline::ServiceSpec;
        let step = step_with_attempt("run-1", "build", "a1");
        let fetch = sample_fetch();
        let workspace_pod = |spec: &StepSpec| {
            build_pod(
                "scarab-x",
                "ns",
                &step,
                spec,
                None,
                DEFAULT_STEP_TIMEOUT_SECS,
                true,
                DEFAULT_CLONE_IMAGE,
                Some(&fetch),
            )
        };

        // A stock root image (e.g. `rust:1-bookworm`) forces `run_as_root`, and the
        // step builds into the restored workspace.
        let mut spec = busybox();
        spec.run_as_root = true;
        spec.workspace_inputs = vec!["tree-from-clone".into()];
        let ps = workspace_pod(&spec).spec.unwrap();
        let psc = ps.security_context.as_ref().expect("pod security context");
        assert_eq!(psc.fs_group, Some(65532));
        assert_eq!(
            psc.supplemental_groups,
            Some(vec![65532]),
            "a run_as_root step must be in the workspace-owning group — it has no \
             DAC_OVERRIDE to fall back on"
        );
        // ...and it is still the restricted sandbox: no capability was handed back.
        let sc = ps.containers[0].security_context.as_ref().unwrap();
        assert_eq!(
            sc.capabilities.as_ref().unwrap().drop,
            Some(vec!["ALL".to_string()])
        );
        assert!(sc.capabilities.as_ref().unwrap().add.is_none());
        assert_eq!(sc.privileged, Some(false));
        assert_eq!(sc.allow_privilege_escalation, Some(false));

        // A sidecar service that pins its own non-root uid takes the Pod-level
        // fsGroup (it needs its data emptyDir chowned) — the step's workspace
        // group membership must NOT be collateral damage.
        let mut spec = busybox();
        spec.workspace_inputs = vec!["tree-from-clone".into()];
        spec.services = vec![ServiceSpec {
            image: "postgres:16".into(),
            ports: vec![5432],
            run_as_user: Some(999),
            ..Default::default()
        }];
        let psc = workspace_pod(&spec)
            .spec
            .unwrap()
            .security_context
            .expect("pod security context");
        assert_eq!(psc.fs_group, Some(999), "the service still wins fsGroup");
        assert!(
            psc.supplemental_groups
                .as_ref()
                .is_some_and(|g| g.contains(&65532)),
            "declaring a service must not cost the step its workspace group: {:?}",
            psc.supplemental_groups
        );

        // No workspace => no workspace group (unchanged shape).
        let ps = build_pod(
            "scarab-x",
            "ns",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        )
        .spec
        .unwrap();
        assert!(ps.security_context.is_none());
    }

    #[test]
    fn build_pod_clone_step_shape() {
        use scarab_engine::{CloneConfig, CloneCredential};
        let step = step_with_attempt("run-1", "checkout", "a1");
        let mut spec = busybox();
        spec.image = String::new();
        spec.command = vec![];
        spec.clone = Some(CloneConfig {
            owner: "acme".into(),
            name: "web".into(),
            sha: "cafe1234".into(),
            depth_full: true,
            submodules: true,
            lfs: false,
            read_only: true,
            url: "https://github.com/acme/web.git".into(),
            credential: Some(CloneCredential {
                username: "x-access-token".into(),
                token: "sekret-token".into(),
            }),
        });
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            "ghcr.io/acme/scarab-clone@sha256:abc",
            None,
        );
        let c = &pod.spec.as_ref().unwrap().containers[0];
        // The canonical image, never the author's; entrypoint from the image.
        assert_eq!(
            c.image.as_deref(),
            Some("ghcr.io/acme/scarab-clone@sha256:abc")
        );
        assert!(c.command.is_none());
        let env = c.env.as_ref().unwrap();
        let get = |k: &str| {
            env.iter()
                .find(|e| e.name == k)
                .and_then(|e| e.value.clone())
        };
        assert_eq!(
            get("SCARAB_CLONE_URL").as_deref(),
            Some("https://github.com/acme/web.git")
        );
        assert_eq!(get("SCARAB_CLONE_SHA").as_deref(), Some("cafe1234"));
        assert_eq!(get("SCARAB_CLONE_DEPTH").as_deref(), Some("full"));
        assert_eq!(get("SCARAB_CLONE_SUBMODULES").as_deref(), Some("true"));
        assert_eq!(get("SCARAB_CLONE_LFS").as_deref(), Some("false"));
        assert_eq!(
            get("SCARAB_CLONE_TOKEN_FILE").as_deref(),
            Some("/scarab/secrets/clone-token")
        );
        // THE INVARIANT (ADR-0045): the token appears in NO env var — tmpfs only.
        assert!(
            env.iter()
                .all(|e| e.value.as_deref() != Some("sekret-token")),
            "token must never ride in env"
        );
        // The tmpfs secret volume is mounted read-only at /scarab/secrets.
        let mounts = c.volume_mounts.as_ref().unwrap();
        let m = mounts
            .iter()
            .find(|m| m.name == "scarab-clone-token")
            .unwrap();
        assert_eq!(m.mount_path, "/scarab/secrets");
        assert_eq!(m.read_only, Some(true));
        let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let v = vols
            .iter()
            .find(|v| v.name == "scarab-clone-token")
            .unwrap();
        assert_eq!(
            v.secret.as_ref().unwrap().secret_name.as_deref(),
            Some("scarab-x-token")
        );

        // Anonymous clone (no credential): no token volume, no TOKEN_FILE env.
        spec.clone.as_mut().unwrap().credential = None;
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            "img",
            None,
        );
        let c = &pod.spec.as_ref().unwrap().containers[0];
        assert!(c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .all(|e| e.name != "SCARAB_CLONE_TOKEN_FILE"));
        assert!(pod
            .spec
            .as_ref()
            .unwrap()
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .all(|v| v.name != "scarab-clone-token"));
    }

    #[test]
    fn build_pod_sets_the_step_deadline() {
        let step = step_with_attempt("run-1", "build", "a1");
        // Default: the global default deadline.
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &busybox(),
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        assert_eq!(
            pod.spec.as_ref().unwrap().active_deadline_seconds,
            Some(DEFAULT_STEP_TIMEOUT_SECS as i64),
        );
        // Authored `timeout:` overrides it (ADR-0047).
        let mut spec = busybox();
        spec.timeout_seconds = Some(120);
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            false,
            DEFAULT_CLONE_IMAGE,
            None,
        );
        assert_eq!(
            pod.spec.as_ref().unwrap().active_deadline_seconds,
            Some(120)
        );
    }

    #[test]
    fn pod_state_fails_fast_on_terminal_waiting_reason() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateWaiting, ContainerStatus, PodStatus,
        };

        let waiting = |reason: &str| Pod {
            status: Some(PodStatus {
                // A stuck container keeps the Pod in Pending.
                phase: Some("Pending".into()),
                container_statuses: Some(vec![ContainerStatus {
                    name: "step".into(),
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some(reason.into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };

        // Un-recoverable container/image errors surface as a terminal failure so
        // the run does not hang forever (and the log tail stops retrying). The
        // class splits by whether re-running the identical spec could ever help.

        // Permanent config/admission rejections: fail fast as a developer
        // verdict (`Config`), never auto-retried (ADR-0047).
        for reason in [
            "CreateContainerConfigError",
            "CreateContainerError",
            "RunContainerError",
            "InvalidImageName",
        ] {
            assert_eq!(
                pod_state(&waiting(reason)),
                ExecState::Failed {
                    exit_code: None,
                    class: FailureClass::Config,
                    cause: None,
                },
                "{reason} is a permanent config rejection"
            );
        }

        // Image-pull failures may be a transient registry/network blip: the main
        // process never ran, so never-started infra — safe to bounded-auto-retry.
        for reason in ["ErrImagePull", "ImagePullBackOff"] {
            assert_eq!(
                pod_state(&waiting(reason)),
                ExecState::Failed {
                    exit_code: None,
                    class: FailureClass::Infra {
                        never_started: true
                    },
                    cause: None,
                },
                "{reason} is (possibly transient) never-started infra"
            );
        }

        // A benign transient wait (e.g. still pulling) stays Pending.
        assert_eq!(pod_state(&waiting("ContainerCreating")), ExecState::Pending);
        assert_eq!(pod_state(&waiting("PodInitializing")), ExecState::Pending);
    }

    /// One fixture per ADR-0047 classification rule.
    mod failure_classification {
        use super::*;
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStatus,
            PodCondition, PodStatus,
        };

        fn class_of(pod: &Pod) -> FailureClass {
            match pod_state(pod) {
                ExecState::Failed { class, .. } => class,
                other => panic!("expected Failed, got {other:?}"),
            }
        }

        fn failed_pod(reason: Option<&str>, container: Option<ContainerStatus>) -> Pod {
            Pod {
                status: Some(PodStatus {
                    phase: Some("Failed".into()),
                    reason: reason.map(String::from),
                    container_statuses: container.map(|c| vec![c]),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        fn terminated(exit_code: i32, reason: Option<&str>) -> ContainerStatus {
            ContainerStatus {
                name: STEP_CONTAINER.into(),
                state: Some(ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        exit_code,
                        reason: reason.map(String::from),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        #[test]
        fn deadline_exceeded_is_timeout() {
            let pod = failed_pod(Some("DeadlineExceeded"), Some(terminated(137, None)));
            assert_eq!(class_of(&pod), FailureClass::Timeout);
        }

        #[test]
        fn oom_kill_is_post_start_infra_despite_exit_code() {
            let pod = failed_pod(None, Some(terminated(137, Some("OOMKilled"))));
            assert_eq!(
                class_of(&pod),
                FailureClass::Infra {
                    never_started: false
                },
                "the platform killed the process — not the step's verdict"
            );
        }

        #[test]
        fn evicted_while_running_is_post_start_infra() {
            // An evicted Pod whose step container had started (terminated state
            // exists) — a side effect may have occurred.
            let pod = failed_pod(Some("Evicted"), Some(terminated(137, None)));
            assert_eq!(
                class_of(&pod),
                FailureClass::Infra {
                    never_started: false
                }
            );
        }

        #[test]
        fn evicted_before_start_is_never_started_infra() {
            // Evicted with no container ever started: no side effect possible.
            let pod = failed_pod(Some("Evicted"), None);
            assert_eq!(
                class_of(&pod),
                FailureClass::Infra {
                    never_started: true
                }
            );
        }

        #[test]
        fn verdictless_failed_snapshot_defers_classification() {
            // phase=Failed but the kubelet hasn't written the verdict yet (no
            // reason, no exit code, no terminated state) — classification is
            // deferred to the next poll rather than misread (observed on k3s
            // enforcing activeDeadlineSeconds).
            let pod = failed_pod(None, None);
            assert_eq!(pod_state(&pod), ExecState::Pending);
        }

        #[test]
        fn running_container_counts_as_started() {
            // Failed phase but the container status still shows `running`
            // (point-in-time race): post-start.
            let container = ContainerStatus {
                name: STEP_CONTAINER.into(),
                state: Some(ContainerState {
                    running: Some(ContainerStateRunning::default()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let pod = failed_pod(Some("Evicted"), Some(container));
            assert_eq!(
                class_of(&pod),
                FailureClass::Infra {
                    never_started: false
                }
            );
        }

        #[test]
        fn unschedulable_is_never_started_infra() {
            let pod = Pod {
                status: Some(PodStatus {
                    phase: Some("Pending".into()),
                    conditions: Some(vec![PodCondition {
                        type_: "PodScheduled".into(),
                        status: "False".into(),
                        reason: Some("Unschedulable".into()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert_eq!(
                pod_state(&pod),
                ExecState::Failed {
                    exit_code: None,
                    class: FailureClass::Infra {
                        never_started: true
                    },
                    cause: None,
                }
            );
        }

        #[test]
        fn scheduled_pending_pod_stays_pending() {
            // PodScheduled=True (or no verdict yet) must NOT fail the step.
            let pod = Pod {
                status: Some(PodStatus {
                    phase: Some("Pending".into()),
                    conditions: Some(vec![PodCondition {
                        type_: "PodScheduled".into(),
                        status: "True".into(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert_eq!(pod_state(&pod), ExecState::Pending);
        }
    }

    /// The drain's completeness guard (git-bug `a3e7845`).
    ///
    /// These are unit tests of [`strip_stream_sentinel`] and [`framed_command`]
    /// rather than of a live drain, and that is the point: the hazard is not a
    /// Kubernetes behaviour, it is that **a truncated tar is a valid tar**. Prove
    /// that once, in-process, and the guard that stands in front of it is pinned
    /// for every caller. The live tier's job is the wiring, not the arithmetic.
    mod stream_framing {
        use super::*;

        /// One tar entry: a 512-byte header plus its payload padded to 512, for the
        /// sub-512-byte files below. Cutting the stream at a multiple of this is
        /// cutting it at a record boundary, which is what a killed `tar` does.
        const ENTRY_BYTES: usize = 1024;

        /// A tar of three small files, entries appended in a KNOWN order so the
        /// truncation offsets below are arithmetic rather than guesswork.
        fn tar_of_three_files() -> Vec<u8> {
            let mut builder = tar::Builder::new(Vec::new());
            for name in ["a.txt", "b.txt", "c.txt"] {
                let body = format!("contents of {name}\n").into_bytes();
                let mut header = tar::Header::new_gnu();
                header.set_path(name).expect("path");
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, body.as_slice()).expect("append");
            }
            builder.into_inner().expect("finish")
        }

        fn unpack_count(bytes: &[u8]) -> Result<usize, String> {
            let out = tempfile::tempdir().expect("tmp");
            unpack_dir(bytes, out.path())?;
            Ok(std::fs::read_dir(out.path())
                .expect("read_dir")
                .filter_map(Result::ok)
                .filter(|e| e.path().is_file())
                .count())
        }

        /// THE HAZARD, demonstrated rather than asserted about. A tar stream cut at
        /// a 512-byte record boundary — an OOM-killed egress container, node
        /// pressure, a `SIGPIPE` — unpacks **without error** into a tree that is
        /// missing files. Nothing downstream can tell that apart from a small
        /// workspace: `Cas::ingest` hashes what it is given, and the Attempt
        /// publishes it as its authoritative snapshot with a green verdict.
        ///
        /// If this test ever starts failing because the `tar` crate learned to
        /// reject a short archive, that is good news — but the guard below is what
        /// this codebase relies on, not that.
        #[test]
        fn a_truncated_tar_unpacks_into_a_partial_tree_without_erroring() {
            let full = tar_of_three_files();
            assert_eq!(unpack_count(&full), Ok(3), "the whole archive has 3 files");

            // Keep the first entry's header + payload and nothing else.
            assert!(full.len() > ENTRY_BYTES);
            let truncated = full[..ENTRY_BYTES].to_vec();

            let partial = unpack_count(&truncated).expect(
                "a truncated tar is a VALID tar to the reader — this is the silent data loss",
            );
            assert!(
                partial < 3,
                "the truncated stream must have produced fewer files than it claimed to carry, \
                 got {partial}"
            );
        }

        /// The regression test for `a3e7845`: the same truncated stream, now framed,
        /// is refused **before** anything unpacks it. Fail-closed — the caller turns
        /// this into a transient drive error, which withholds the verdict rather
        /// than publishing a partial root.
        #[test]
        fn a_truncated_capture_is_rejected_before_it_can_be_unpacked() {
            let sentinel = stream_sentinel();
            let full = tar_of_three_files();
            // What the wire delivers when `tar` dies partway: a prefix of the
            // payload, and — because `framed_command` joins with `&&` — no
            // sentinel, because `printf` never ran.
            let truncated = full[..ENTRY_BYTES].to_vec();

            let err = strip_stream_sentinel(truncated, &sentinel)
                .expect_err("an unterminated stream must not be handed back as a payload");
            assert!(
                err.contains("incomplete"),
                "the error must name the reason: {err}"
            );
        }

        /// An empty capture is the same defect, not a benign empty workspace: a
        /// drain that produced nothing at all still owes its sentinel.
        #[test]
        fn an_empty_capture_is_rejected() {
            let sentinel = stream_sentinel();
            assert!(strip_stream_sentinel(Vec::new(), &sentinel).is_err());
        }

        /// A complete capture yields the payload byte-for-byte — the sentinel is
        /// framing, and must not survive into the bytes that get unpacked.
        #[test]
        fn a_complete_capture_round_trips_to_the_exact_payload() {
            let sentinel = stream_sentinel();
            let payload = tar_of_three_files();
            let mut wire = payload.clone();
            wire.extend_from_slice(sentinel.as_bytes());

            let stripped = strip_stream_sentinel(wire, &sentinel).expect("complete");
            assert_eq!(stripped, payload);
            assert_eq!(unpack_count(&stripped), Ok(3));
        }

        /// A sentinel that merely *appears* in the payload proves nothing: the guard
        /// is about the stream's END. (Contrived — the marker is unique per exec —
        /// but it is the property being claimed, so it is the property tested.)
        #[test]
        fn a_sentinel_in_the_middle_does_not_satisfy_the_guard() {
            let sentinel = stream_sentinel();
            let mut wire = sentinel.as_bytes().to_vec();
            wire.extend_from_slice(b"...more bytes were still arriving...");
            assert!(strip_stream_sentinel(wire, &sentinel).is_err());
        }

        /// The `&&` is the mechanism, so it is asserted: a shape that ran `printf`
        /// unconditionally would emit a sentinel for a `tar` that had just died,
        /// which is precisely the bug wearing the fix's clothes.
        #[test]
        fn the_framed_command_emits_the_sentinel_only_on_success() {
            let framed = framed_command("tar -cf - -C /workspace .", "__mark__");
            assert_eq!(
                framed,
                "tar -cf - -C /workspace . && printf '%s' '__mark__'",
                "the payload command must gate the sentinel"
            );
            assert!(!framed.contains(';'), "`;` would run printf regardless");
        }

        /// Unique per call: a leftover sentinel from an earlier exec on the same Pod
        /// must never be able to vouch for this one's stream.
        #[test]
        fn sentinels_are_unique_per_capture() {
            let a = stream_sentinel();
            let b = stream_sentinel();
            assert_ne!(a, b);
            // Shell-safe inside the single quotes `framed_command` puts them in.
            for s in [&a, &b] {
                assert!(
                    s.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
                    "{s} must not need shell quoting"
                );
            }
        }
    }
}
