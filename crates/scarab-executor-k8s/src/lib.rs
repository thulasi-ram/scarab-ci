//! Kubernetes adapter for the [`scarab_engine::Executor`] port (ADR-0004).
//!
//! Each Step runs as **one bare Pod** with `restartPolicy: Never` — a clean,
//! individually-addressable, re-creatable object. The orchestrator owns retries
//! (ADR-0020); this adapter only creates the Pod, reflects its status, and
//! deletes it on cancel. `launch` is **idempotent on the step's fence**: the Pod
//! name is derived deterministically from `{run, step, attempt}`, so a relaunch
//! after a control-plane crash re-attaches to the existing Pod rather than
//! starting a second one (the double-effect guard, ADR-0021).

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
use scarab_storage::Cas;

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
    /// `/workspace` machinery: an init container that receives the merged
    /// `needs` workspaces (materialized by the control plane and streamed in
    /// over exec), and an egress sidecar the control plane snapshots back
    /// into the CAS after the step exits. `None` = no workspace flow (tests /
    /// object-store-less dev).
    workspace_cas: Option<Arc<dyn Cas>>,
    /// The artifact blob store (ADR-0052). When wired (and the workspace
    /// flow is on), every step Pod gets a `/scarab/artifacts` emptyDir that
    /// is harvested post-step: matching files upload as object blobs and the
    /// metadata rides a Pod annotation the orchestrator persists.
    artifact_store: Option<Arc<dyn scarab_storage::ObjectStore>>,
    /// Operator placement config (ADR-0055): baseline + PlacementProfile registry.
    placement: PlacementConfig,
}

impl K8sExecutor {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            client: None,
            namespace: namespace.into(),
            results_egress: None,
            default_step_timeout_secs: DEFAULT_STEP_TIMEOUT_SECS,
            workspace_cas: None,
            artifact_store: None,
            clone_image: DEFAULT_CLONE_IMAGE.to_string(),
            placement: PlacementConfig::default(),
        }
    }

    pub fn with_client(namespace: impl Into<String>, client: kube::Client) -> Self {
        Self {
            client: Some(client),
            namespace: namespace.into(),
            results_egress: None,
            default_step_timeout_secs: DEFAULT_STEP_TIMEOUT_SECS,
            workspace_cas: None,
            artifact_store: None,
            clone_image: DEFAULT_CLONE_IMAGE.to_string(),
            placement: PlacementConfig::default(),
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

    /// Enable the workspace CAS flow (ADR-0029/0045): materialize `needs`
    /// workspaces into each step Pod and snapshot `/workspace` back after it
    /// exits.
    pub fn with_workspace_cas(mut self, cas: Arc<dyn Cas>) -> Self {
        self.workspace_cas = Some(cas);
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
        spec: &StepSpec,
    ) -> Result<(), ExecError> {
        self.ensure_clone_secret(pod_name, pod, spec).await?;
        self.ensure_registry_secret(pod_name, pod, spec).await?;
        self.ensure_oidc_secret(pod_name, pod, spec).await?;
        Ok(())
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

    /// Drive the workspace lifecycle for `pod` (ADR-0029/0045). Two idempotent
    /// legs, all state derived from the Pod itself:
    ///
    /// 1. **Feed** — while the `scarab-workspace-init` container is running
    ///    (it waits on a marker), materialize the input CAS roots (from the
    ///    Pod annotation) into a temp dir, stream them in as a tar over exec,
    ///    and touch the marker. Only content the Pod lacks moves: the init
    ///    container only ever runs before the step, exactly once per Pod.
    /// 2. **Snapshot** — once the step container has terminated and the
    ///    egress sidecar is still holding the Pod open, tar `/workspace` out
    ///    (successful steps only), `Cas::ingest` it (per-file merkle dedup —
    ///    unchanged blobs upload nothing), patch the root onto the Pod as an
    ///    annotation, then release the sidecar.
    async fn drive_workspace(
        &self,
        pods: &Api<Pod>,
        pod: &Pod,
        cas: &dyn Cas,
    ) -> Result<(), DriveErr> {
        let name = pod.metadata.name.clone().ok_or("pod has no name")?;
        let annotations = pod.metadata.annotations.clone().unwrap_or_default();
        // Not a workspace Pod (built before the CAS was wired) — nothing to do.
        let Some(inputs_csv) = annotations.get(ANNOTATION_WS_INPUTS) else {
            return Ok(());
        };

        // --- Leg 1: feed the waiting init container. -----------------------
        if init_container_running(pod, WORKSPACE_INIT_CONTAINER) && !inputs_csv.is_empty() {
            let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
            for root in inputs_csv.split(',').filter(|r| !r.is_empty()) {
                // Later inputs overlay earlier ones (merge-in-order, ADR-0007).
                cas.materialize(
                    &scarab_storage::TreeHash(root.to_string()),
                    tmp.path().to_str().ok_or("tmp path")?,
                )
                .await
                .map_err(|e| match e {
                    // Permanent: the input workspace blob is gone from the object
                    // store (evicted, or — the classic dev footgun — the store
                    // lived on an emptyDir that a restart wiped). The step can
                    // never be provisioned, so fail the attempt fast rather than
                    // leave the `scarab-workspace-init` barrier waiting forever.
                    scarab_storage::StorageError::NotFound => DriveErr::InputMissing(format!(
                        "workspace input {root} is missing from the object store — cannot \
                         provision this step (blob evicted or the store was wiped)"
                    )),
                    other => DriveErr::Transient(format!("materialize {root}: {other}")),
                })?;
            }
            let tar_bytes = pack_dir(tmp.path())?;
            // The CAS tarball carries the server's uid/gid/mode (65532, 0755) on
            // every entry incl. `.`, so extracting it resets /workspace itself to
            // `65532:65532 0755` — clobbering the group-writability that fsGroup
            // (also 65532) set up at mount time. A `run_as_root` step (uid 0, all
            // caps incl. DAC_OVERRIDE dropped), or any non-root image whose uid
            // isn't 65532, is then a member of group 65532 via fsGroup but can't
            // write the group-unwritable tree (b04697f: `Permission denied` on
            // e.g. cargo's target dir). Restore group write (g+w) so every step
            // process — which is always in group 65532 — can write, and setgid on
            // dirs (g+s) so files it creates stay in group 65532 for the egress
            // snapshot. Grants no capability/ownership the operator didn't ask for.
            self.exec_with_stdin(
                pods,
                &name,
                WORKSPACE_INIT_CONTAINER,
                &format!(
                    "tar -xf - -C {WORKSPACE_MOUNT_PATH} \
                     && chmod -R g+rwX {WORKSPACE_MOUNT_PATH} \
                     && find {WORKSPACE_MOUNT_PATH} -type d -exec chmod g+s {{}} ';' \
                     && touch {CTL_MOUNT_PATH}/init-done"
                ),
                tar_bytes,
            )
            .await?;
        }

        // --- Leg 2: snapshot after the step exits. --------------------------
        let step_exit = step_terminated_exit(pod);
        if let Some(exit) = step_exit {
            if init_container_running(pod, WORKSPACE_EGRESS_CONTAINER) {
                let already = annotations
                    .get(ANNOTATION_WS_ROOT)
                    .is_some_and(|v| !v.is_empty());
                if exit == 0 && !already {
                    let out = self
                        .exec_capture_stdout(
                            pods,
                            &name,
                            WORKSPACE_EGRESS_CONTAINER,
                            &format!("tar -cf - -C {WORKSPACE_MOUNT_PATH} ."),
                        )
                        .await?;
                    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
                    unpack_dir(&out, tmp.path())?;
                    let snapshot = cas
                        .ingest(tmp.path().to_str().ok_or("tmp path")?)
                        .await
                        .map_err(|e| format!("ingest: {e}"))?;
                    // Per-path publishing (ADR-0007): restrict the published root
                    // to the authored `outputs:` paths. The whole workspace is
                    // still ingested first — blobs are shared, so pruning is a
                    // tree rebuild that uploads nothing new, and the step's own
                    // files stay recoverable from the full snapshot if we ever
                    // want them. A declared path the step did not produce is a
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
                    let published = if declared.is_empty() {
                        snapshot.root
                    } else {
                        scarab_storage::prune_tree(cas, &snapshot.root, &declared)
                            .await
                            .map_err(|e| match e {
                                scarab_storage::PruneError::Storage(e) => {
                                    DriveErr::Transient(format!("prune outputs: {e}"))
                                }
                                permanent => DriveErr::OutputContract(format!(
                                    "outputs: {permanent} (declared: {})",
                                    declared.join(", ")
                                )),
                            })?
                    };
                    // Record the root on the Pod BEFORE releasing the sidecar:
                    // output() reads it durably across control-plane restarts.
                    let patch = serde_json::json!({
                        "metadata": { "annotations": { ANNOTATION_WS_ROOT: published.0 } }
                    });
                    pods.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                        .await
                        .map_err(|e| format!("annotate root: {e}"))?;
                }
                // Harvest artifacts of record (ADR-0052): everything the
                // step wrote to /scarab/artifacts, filtered by its authored
                // globs, uploaded as plain object blobs (NOT the CAS — an
                // independent lifecycle) and indexed on the Pod annotation.
                //
                // The upload is a real, already-committed effect, so its INDEX
                // must be equally durable: a harvest error is TRANSIENT, exactly
                // like the workspace snapshot above, and returns before the
                // sidecar is released. That keeps the settle barrier closed, so
                // the next poll re-harvests (idempotent: same object keys, and
                // the annotation guard makes a completed harvest once-only) and
                // the Succeeded verdict stays withheld meanwhile (see `poll`).
                // Swallowing the error instead released the barrier and reported
                // success with the blobs uploaded and NOTHING indexed (98ea804).
                // Retries are bounded by the engine's step-timeout backstop: a
                // permanently failing harvest settles the attempt as Timeout —
                // it never reports success having lost the index.
                if let Some(store) = &self.artifact_store {
                    if artifact_harvest_owed(pod, true) {
                        self.harvest_artifacts(pods, pod, &name, store.as_ref())
                            .await
                            .map_err(|e| DriveErr::Transient(format!("artifact harvest: {e}")))?;
                    }
                }
                // Release the sidecar (idempotent): failed steps snapshot
                // nothing — their workspace is not an output.
                self.exec_with_stdin(
                    pods,
                    &name,
                    WORKSPACE_EGRESS_CONTAINER,
                    &format!("touch {CTL_MOUNT_PATH}/egress-done"),
                    Vec::new(),
                )
                .await?;
            }
        }
        Ok(())
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
        let out = self
            .exec_capture_stdout(
                pods,
                name,
                WORKSPACE_EGRESS_CONTAINER,
                &format!("tar -cf - -C {ARTIFACTS_MOUNT_PATH} . 2>/dev/null || true"),
            )
            .await?;
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        if !out.is_empty() {
            let _ = unpack_dir(&out, tmp.path()); // an empty dir tars to nothing
        }

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
                metas.push(scarab_engine::ArtifactMeta {
                    name: rel.clone(),
                    size: bytes.len() as u64,
                    content_type: content_type_of(&rel).to_string(),
                    object_key: key,
                });
            }
        }
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
        // Best-effort join: when this exec's command releases a wait-loop
        // (touching a marker), the container exits immediately and the exec's
        // status frame is torn down with it — a benign race. The workspace
        // state machine is idempotent and derived from the Pod, so a feed
        // that truly failed leaves the init container running and the next
        // poll simply retries.
        let _ = proc.join().await;
        Ok(())
    }

    /// Run `sh -c cmd` in `container`, capturing its stdout.
    async fn exec_capture_stdout(
        &self,
        pods: &Api<Pod>,
        pod: &str,
        container: &str,
        cmd: &str,
    ) -> Result<Vec<u8>, String> {
        use tokio::io::AsyncReadExt as _;
        let params = AttachParams::default()
            .container(container)
            .stdout(true)
            .stderr(false);
        let mut proc = pods
            .exec(pod, ["sh", "-c", cmd], &params)
            .await
            .map_err(|e| format!("exec in {container}: {e}"))?;
        let mut out = Vec::new();
        if let Some(mut stdout) = proc.stdout() {
            stdout
                .read_to_end(&mut out)
                .await
                .map_err(|e| e.to_string())?;
        }
        proc.join().await.map_err(|e| format!("exec join: {e}"))?;
        Ok(out)
    }
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
/// Why a `drive_workspace` call failed. `InputMissing` is a **permanent**
/// provisioning failure — a CAS input the step needs is gone, so it can never
/// start and the attempt must fail fast (`poll` deletes the barrier-stuck Pod
/// and reports `Infra { never_started }`). `Transient` is anything else (a
/// store blip, an exec hiccup): `poll` surfaces it as an error and re-drives.
/// `From<String>`/`From<&str>` keep every `?` inside `drive_workspace` — which
/// all yield string errors — compiling unchanged; only the CAS `materialize`
/// call is special-cased to distinguish the two.
enum DriveErr {
    InputMissing(String),
    Transient(String),
    /// A declared `outputs:` path was not produced (or is not a legal
    /// workspace-relative path) — permanent and author-fixable, so it fails the
    /// step with a developer verdict instead of retrying (ADR-0007 fail-closed).
    OutputContract(String),
}

impl std::fmt::Display for DriveErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriveErr::InputMissing(s)
            | DriveErr::Transient(s)
            | DriveErr::OutputContract(s) => f.write_str(s),
        }
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
///
/// The settle (workspace snapshot + artifact harvest) runs in the egress NATIVE
/// sidecar, which does NOT gate the Pod phase: a workspace Pod reports
/// `phase=Succeeded` the instant the step container exits, BEFORE settle has
/// patched the annotations that `output()`/`artifacts()` read. The orchestrator
/// indexes those on the terminal verdict exactly once, so reporting Succeeded
/// early loses them permanently (98ea804). `drive_workspace` touches
/// `egress-done` only after patching, so that sidecar still running IS
/// "settle incomplete": withhold ONLY the Succeeded verdict while it is.
///
/// Every other verdict passes through verbatim — an infra failure must never be
/// masked as Running — and the scheduler's next poll drives settle to done, so
/// this is deterministic rather than a sleep.
fn settled_state(pod: &Pod) -> ExecState {
    let state = pod_state(pod);
    if matches!(state, ExecState::Succeeded)
        && init_container_running(pod, WORKSPACE_EGRESS_CONTAINER)
    {
        return ExecState::Running;
    }
    state
}

/// Whether the settle leg still owes an artifact harvest for this Pod (ADR-0052)
/// — pure and derived entirely from the Pod, so it survives a control-plane
/// restart and every re-poll reaches the same verdict.
///
/// The egress barrier must stay closed while this is true: releasing it lets the
/// Pod report `phase=Succeeded`, and the orchestrator indexes a step's artifacts
/// off that verdict exactly once (98ea804). It flips false only once the harvest
/// has recorded its index on the Pod — including the EMPTY index of a step that
/// published nothing — which is also what makes a re-harvest once-only.
fn artifact_harvest_owed(pod: &Pod, harvesting: bool) -> bool {
    harvesting
        && step_terminated_exit(pod) == Some(0)
        && !pod
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|a| a.contains_key(ANNOTATION_ARTIFACTS))
}

/// Pack `dir` into an uncompressed tar (workspaces move within one cluster
/// hop; compression buys little and costs CPU).
fn pack_dir(dir: &std::path::Path) -> Result<Vec<u8>, String> {
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_dir_all(".", dir)
        .map_err(|e| format!("tar pack: {e}"))?;
    builder.into_inner().map_err(|e| format!("tar finish: {e}"))
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

        // Re-attach: if the Pod already exists (a prior launch we may not have
        // observed completing), adopt it instead of creating a duplicate.
        let existing = pods
            .get_opt(&name)
            .await
            .map_err(|e| ExecError::Launch(e.to_string()))?;
        if let Some(pod) = existing {
            // Refresh the short-TTL clone credential on re-drives (ADR-0045).
            self.ensure_step_secrets(&name, &pod, spec).await?;
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
        );
        // ADR-0055: stamp the baseline, merge the named PlacementProfiles + the
        // governed k8s_overlay, and apply requested resources — fallible because a
        // named profile might not exist in the registry (fail-closed).
        let pod = apply_placement(pod, spec, &self.placement).map_err(ExecError::Launch)?;
        match pods.create(&PostParams::default(), &pod).await {
            Ok(created) => {
                self.ensure_step_secrets(&name, &created, spec).await?;
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
                    self.ensure_step_secrets(&name, &pod, spec).await?;
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
                    match self.drive_workspace(&pods, &pod, cas.as_ref()).await {
                        Ok(()) => {}
                        // Permanent: an input workspace is gone from the object
                        // store, so this step can never be provisioned. Delete
                        // the Pod stuck on the workspace-init barrier and fail
                        // the attempt fast — `Infra { never_started: true }`
                        // (the main process never ran ⇒ no side effect) — with a
                        // clear reason, instead of the init container waiting
                        // forever. Bounded retries then dead-letter deterministically.
                        Err(DriveErr::InputMissing(msg)) => {
                            eprintln!("scarab-executor: {msg} (pod {})", handle.0);
                            let _ = pods.delete(&handle.0, &DeleteParams::default()).await;
                            return Ok(ExecState::Failed {
                                exit_code: None,
                                class: FailureClass::Infra {
                                    never_started: true,
                                },
                            });
                        }
                        // Permanent and author-fixable (ADR-0007): the step ran and
                        // exited 0, but did not honor its declared `outputs:`
                        // contract. Retrying the identical spec cannot fix it, so
                        // fail fast with a developer verdict (`Config`) rather than
                        // burn the infra retry budget and dead-letter as an
                        // operator problem it is not. The Pod is left for its logs.
                        Err(DriveErr::OutputContract(msg)) => {
                            eprintln!("scarab-executor: {msg} (pod {})", handle.0);
                            return Ok(ExecState::Failed {
                                exit_code: None,
                                class: FailureClass::Config,
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
                    // reports phase=Succeeded the instant the step exits — BEFORE
                    // the settle has patched them, which would let the scheduler
                    // read an empty artifact set exactly once and index nothing
                    // (98ea804). drive_workspace touches egress-done only AFTER
                    // patching, so the sidecar terminating is the settle-complete
                    // signal: re-read and withhold ONLY the Succeeded verdict
                    // while that sidecar is still running. Every other verdict
                    // passes through verbatim so infra failures are never masked;
                    // the scheduler re-polls next tick and drives settle to done
                    // (deterministic, no sleeps).
                    let pod = match pods
                        .get_opt(&handle.0)
                        .await
                        .map_err(|e| ExecError::Other(e.to_string()))?
                    {
                        Some(pod) => pod,
                        None => return Ok(ExecState::Lost),
                    };
                    return Ok(settled_state(&pod));
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
        let ctl_mount = VolumeMount {
            name: CTL_VOLUME.to_string(),
            mount_path: CTL_MOUNT_PATH.to_string(),
            ..Default::default()
        };
        // With a snapshot, the init container waits for the control-plane feed;
        // without one, it exits immediately and the shell starts empty.
        let wait = if snapshot_root.is_some() {
            format!("until [ -f {CTL_MOUNT_PATH}/init-done ]; do sleep 0.2; done")
        } else {
            "exit 0".to_string()
        };
        let init = Container {
            name: WORKSPACE_INIT_CONTAINER.to_string(),
            image: Some(WORKSPACE_HELPER_IMAGE.to_string()),
            command: Some(vec!["sh".into(), "-c".into(), wait]),
            volume_mounts: Some(vec![ws_mount.clone(), ctl_mount.clone()]),
            ..Default::default()
        };
        let shell = Container {
            name: STEP_CONTAINER.to_string(),
            image: Some(image.to_string()),
            // Keep the Pod alive so it can be shelled into; TTL-bounded.
            command: Some(vec!["sleep".into(), ttl_secs.to_string()]),
            working_dir: Some(WORKSPACE_MOUNT_PATH.to_string()),
            volume_mounts: Some(vec![ws_mount, ctl_mount]),
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
                init_containers: Some(vec![init]),
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
                volumes: Some(vec![
                    Volume {
                        name: WORKSPACE_VOLUME.to_string(),
                        empty_dir: Some(EmptyDirVolumeSource::default()),
                        ..Default::default()
                    },
                    Volume {
                        name: CTL_VOLUME.to_string(),
                        empty_dir: Some(EmptyDirVolumeSource::default()),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        pods.create(&PostParams::default(), &pod)
            .await
            .map_err(|e| ExecError::Other(format!("create debug pod: {e}")))?;

        // Re-materialize the step's output snapshot into /workspace (same feed
        // path as a real step's inputs).
        if let Some(root) = snapshot_root {
            let cas = self.workspace_cas.clone().ok_or(ExecError::Unavailable)?;
            self.wait_container(&pods, &name, WORKSPACE_INIT_CONTAINER, true, 90)
                .await?;
            let tmp = tempfile::tempdir().map_err(|e| ExecError::Other(e.to_string()))?;
            cas.materialize(
                &scarab_storage::TreeHash(root.to_string()),
                tmp.path()
                    .to_str()
                    .ok_or_else(|| ExecError::Other("tmp path".into()))?,
            )
            .await
            .map_err(|e| ExecError::Other(format!("materialize {root}: {e}")))?;
            let tar = pack_dir(tmp.path()).map_err(ExecError::Other)?;
            self.exec_with_stdin(
                &pods,
                &name,
                WORKSPACE_INIT_CONTAINER,
                &format!(
                    "tar -xf - -C {WORKSPACE_MOUNT_PATH} \
                     && chmod -R g+rwX {WORKSPACE_MOUNT_PATH} \
                     && find {WORKSPACE_MOUNT_PATH} -type d -exec chmod g+s {{}} ';' \
                     && touch {CTL_MOUNT_PATH}/init-done"
                ),
                tar,
            )
            .await
            .map_err(ExecError::Other)?;
        }
        // Wait for the shell container to be running before we let a client attach.
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
/// The workspace `emptyDir` volume name.
const WORKSPACE_VOLUME: &str = "scarab-workspace";
/// Control-plane→Pod handshake dir (markers only; NEVER part of the snapshot).
const CTL_MOUNT_PATH: &str = "/scarab-ctl";
const CTL_VOLUME: &str = "scarab-ctl";
/// The helper image for the workspace init/egress containers: they only run
/// `sh`/`tar`/`sleep`, so a pinned busybox suffices.
const WORKSPACE_HELPER_IMAGE: &str = "busybox:1.36";
/// Names of the workspace helper containers.
const WORKSPACE_INIT_CONTAINER: &str = "scarab-workspace-init";
const WORKSPACE_EGRESS_CONTAINER: &str = "scarab-workspace-egress";
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
/// The Pod annotation carrying the step's authored `outputs:` paths (ADR-0007),
/// comma-separated. Absent/empty = publish the whole workspace. Read at egress,
/// so an adopted Pod prunes identically with no in-memory state.
const ANNOTATION_WS_OUTPUTS: &str = "scarab.io/workspace-outputs";
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
) -> Pod {
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
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        });
        volumes.push(Volume {
            name: CTL_VOLUME.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        });
        // The input CAS roots ride on the Pod itself, so a resumed control
        // plane can feed an adopted Pod with no in-memory state.
        annotations.insert(
            ANNOTATION_WS_INPUTS.to_string(),
            spec.workspace_inputs.join(","),
        );
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
        // Ordinary init container: blocks the step until the control plane
        // has streamed the merged input workspaces in (exits immediately when
        // there is nothing to feed).
        let wait = if spec.workspace_inputs.is_empty() {
            "exit 0".to_string()
        } else {
            format!("until [ -f {CTL_MOUNT_PATH}/init-done ]; do sleep 0.2; done")
        };
        init_containers.push(Container {
            name: WORKSPACE_INIT_CONTAINER.to_string(),
            image: Some(WORKSPACE_HELPER_IMAGE.to_string()),
            command: Some(vec!["sh".into(), "-c".into(), wait]),
            volume_mounts: Some(vec![ws.clone(), ctl.clone()]),
            ..Default::default()
        });
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
                    "trap '' TERM; until [ -f {CTL_MOUNT_PATH}/egress-done ]; do sleep 0.2; done"
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
            if let Some(class) = terminal_waiting_class(pod) {
                ExecState::Failed {
                    exit_code: None,
                    class,
                }
            } else if is_unschedulable(pod) {
                ExecState::Failed {
                    exit_code: None,
                    class: FailureClass::Infra {
                        never_started: true,
                    },
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
        let pod = build_pod(
            "scarab-image-a1",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
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
        );
        assert!(
            !pod.metadata
                .annotations
                .as_ref()
                .unwrap()
                .contains_key(ANNOTATION_WS_OUTPUTS),
            "absent, not empty — an empty annotation would be an ambiguous encoding"
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
        Pod {
            metadata: ObjectMeta {
                name: Some("scarab-x".into()),
                annotations: (!annotations.is_empty()).then_some(annotations),
                ..Default::default()
            },
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
    /// while every failure verdict passes through so infra faults are never masked.
    #[test]
    fn succeeded_is_withheld_until_the_settle_sidecar_has_finished() {
        // Step exited 0, settle still in flight -> NOT terminal yet.
        assert_eq!(
            settled_state(&settling_pod("Succeeded", 0, true, false)),
            ExecState::Running,
            "reporting Succeeded here loses the artifact index permanently"
        );
        // Sidecar gone = settle recorded -> the real verdict.
        assert_eq!(
            settled_state(&settling_pod("Succeeded", 0, false, true)),
            ExecState::Succeeded
        );
        // A failure is never masked as Running, settling or not.
        assert_eq!(
            settled_state(&settling_pod("Failed", 1, true, false)),
            ExecState::Failed {
                exit_code: Some(1),
                class: FailureClass::Step,
            }
        );
    }

    /// 98ea804: the harvest is owed until its index is durably ON the Pod — which
    /// is what lets a failed harvest be retried (transiently, holding the barrier)
    /// instead of releasing the sidecar with the blobs uploaded and nothing
    /// indexed, and what makes a completed harvest once-only across re-polls.
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
            !artifact_harvest_owed(&settling_pod("Failed", 1, true, false), true),
            "a failed step publishes no artifacts of record"
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
        let pod = build_pod(
            "scarab-x",
            "ns",
            &step,
            &spec,
            None,
            DEFAULT_STEP_TIMEOUT_SECS,
            true,
            DEFAULT_CLONE_IMAGE,
        );

        // The input roots ride on the Pod (a resumed control plane feeds an
        // adopted Pod with no in-memory state).
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
        // Init container waits for the feed; egress sidecar (restartPolicy
        // Always) holds the Pod for the snapshot.
        let inits = ps.init_containers.as_ref().unwrap();
        let init = inits
            .iter()
            .find(|c| c.name == WORKSPACE_INIT_CONTAINER)
            .unwrap();
        assert!(init.command.as_ref().unwrap()[2].contains("init-done"));
        let egress = inits
            .iter()
            .find(|c| c.name == WORKSPACE_EGRESS_CONTAINER)
            .unwrap();
        assert_eq!(egress.restart_policy.as_deref(), Some("Always"));
        assert!(egress.command.as_ref().unwrap()[2].contains("egress-done"));
        // The step runs IN the workspace, which is writable via the workspace group.
        assert_eq!(ps.containers[0].working_dir.as_deref(), Some("/workspace"));
        assert_eq!(ps.security_context.as_ref().unwrap().fs_group, Some(65532));
        assert_eq!(ps.termination_grace_period_seconds, Some(600));

        // No inputs => the init container exits immediately (nothing to feed).
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
        );
        let inits = pod.spec.as_ref().unwrap().init_containers.clone().unwrap();
        let init = inits
            .iter()
            .find(|c| c.name == WORKSPACE_INIT_CONTAINER)
            .unwrap();
        assert_eq!(init.command.as_ref().unwrap()[2], "exit 0");

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
        );
        assert!(pod.metadata.annotations.is_none());
        assert!(pod.spec.as_ref().unwrap().init_containers.is_none());
    }

    // git-bug b04697f (dogfood): "clone, then build IN the workspace" — the single
    // most common CI shape — must be able to WRITE the CAS-restored `/workspace`.
    //
    // The control plane materializes the CAS tree as its own uid/gid (65532) and
    // makes it group-writable at feed time, while the ADR-0039 baseline drops ALL
    // capabilities — `DAC_OVERRIDE` included — so *group membership* is the whole
    // mechanism, even for an admitted `run_as_root` step at uid 0. Assert that
    // membership is granted explicitly on every workspace Pod, and that it holds
    // when a uid-pinning sidecar service (ADR-0058) takes the Pod's `fsGroup`.
    #[test]
    fn workspace_steps_are_always_members_of_the_workspace_group() {
        use scarab_pipeline::ServiceSpec;
        let step = step_with_attempt("run-1", "build", "a1");
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
}
