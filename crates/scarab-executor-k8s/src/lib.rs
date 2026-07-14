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
    Capabilities, Container, EmptyDirVolumeSource, EnvVar, Pod, PodSpec, SeccompProfile,
    SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, LogParams, PostParams};

use scarab_engine::ports::{ExecHandle, ExecState, LogChunks};
use scarab_engine::{ExecError, Executor, StepRun, StepSpec};

/// The step container's name in every step Pod (see [`build_pod`]). The log tail
/// pins the source to this container so a results-egress sidecar (ADR-0042) never
/// pollutes the step's log stream.
const STEP_CONTAINER: &str = "step";

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

/// A Kubernetes-backed executor. Holds an optional client so the composition
/// root can construct it without contacting an API server.
pub struct K8sExecutor {
    client: Option<kube::Client>,
    namespace: String,
    results_egress: Option<ResultsEgress>,
}

impl K8sExecutor {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            client: None,
            namespace: namespace.into(),
            results_egress: None,
        }
    }

    pub fn with_client(namespace: impl Into<String>, client: kube::Client) -> Self {
        Self {
            client: Some(client),
            namespace: namespace.into(),
            results_egress: None,
        }
    }

    /// Enable result capture via the per-Pod egress sidecar (ADR-0042).
    pub fn with_results_egress(mut self, egress: ResultsEgress) -> Self {
        self.results_egress = Some(egress);
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
}

#[async_trait]
impl Executor for K8sExecutor {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        let pods = self.pods()?;
        let name = pod_name(step);

        // Re-attach: if the Pod already exists (a prior launch we may not have
        // observed completing), adopt it instead of creating a duplicate.
        if pods
            .get_opt(&name)
            .await
            .map_err(|e| ExecError::Launch(e.to_string()))?
            .is_some()
        {
            return Ok(ExecHandle(name));
        }

        let pod = build_pod(&name, &self.namespace, step, spec, self.results_egress.as_ref());
        match pods.create(&PostParams::default(), &pod).await {
            Ok(_) => Ok(ExecHandle(name)),
            // A concurrent launcher won the race — the Pod now exists, which is
            // exactly what we wanted; treat as a successful re-attach.
            Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(ExecHandle(name)),
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
            Some(pod) => Ok(pod_state(&pod)),
            // The Pod is gone (evicted, GC'd, node lost) — the backend lost it.
            None => Ok(ExecState::Lost),
        }
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
pub fn build_pod(
    name: &str,
    namespace: &str,
    step: &StepRun,
    spec: &StepSpec,
    egress: Option<&ResultsEgress>,
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

    let container = Container {
        name: STEP_CONTAINER.to_string(),
        image: Some(spec.image.clone()),
        command: (!spec.command.is_empty()).then(|| spec.command.clone()),
        env: Some(env),
        security_context: Some(step_security_context(spec)),
        volume_mounts: results_mount.clone().map(|m| vec![m]),
        ..Default::default()
    };

    let labels = std::collections::BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "scarab".to_string(),
        ),
        ("scarab.io/run".to_string(), sanitize_label(&step.run.0)),
        ("scarab.io/step".to_string(), sanitize_label(&step.step.0)),
        ("scarab.io/attempt".to_string(), sanitize_label(&attempt)),
    ]);

    // Egress wiring (ADR-0042): a shared emptyDir + the sidecar as a native
    // sidecar (initContainer with restartPolicy Always), so it starts alongside
    // the step and the kubelet terminates it after the step exits — its window to
    // drain results — without blocking the Pod's terminal phase.
    let (volumes, init_containers) = match egress {
        Some(e) => {
            let volume = Volume {
                name: RESULTS_VOLUME.to_string(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            };
            let sidecar = egress_sidecar(step, &attempt, &results_mount.unwrap(), e);
            (Some(vec![volume]), Some(vec![sidecar]))
        }
        None => (None, None),
    };

    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![container],
            init_containers,
            volumes,
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
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

/// The rootless BuildKit image used by a `kind: build` step (ADR-0018).
const BUILDKIT_IMAGE: &str = "moby/buildkit:rootless";

/// What a built-in `kind: build` step builds (ADR-0018). The context/dockerfile
/// are workspace-relative; `image` is the tag to build and (optionally) push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSpec {
    pub context: String,
    pub dockerfile: String,
    pub image: String,
    pub push: bool,
}

impl BuildSpec {
    /// The `buildctl` args this build compiles to.
    fn buildctl_args(&self) -> Vec<String> {
        vec![
            "build".into(),
            "--frontend".into(),
            "dockerfile.v0".into(),
            "--local".into(),
            format!("context={}", self.context),
            "--local".into(),
            format!("dockerfile={}", self.context),
            "--opt".into(),
            format!("filename={}", self.dockerfile),
            "--output".into(),
            format!("type=image,name={},push={}", self.image, self.push),
        ]
    }
}

/// Build the Pod for a `kind: build` step: **rootless BuildKit** (no
/// Docker-in-Docker, no privileged container) invoking `buildctl` to build the
/// image and push it (ADR-0018). Runs as a non-root user with an unconfined
/// seccomp/AppArmor profile — what rootless buildkitd needs without privilege.
pub fn build_pod_for_build(
    name: &str,
    namespace: &str,
    step: &StepRun,
    build: &BuildSpec,
) -> Pod {
    let attempt = step
        .current_attempt()
        .map(|a| a.id.0.clone())
        .unwrap_or_else(|| "0".to_string());

    let env = vec![
        // Rootless buildkitd cannot use the process sandbox.
        EnvVar {
            name: "BUILDKITD_FLAGS".into(),
            value: Some("--oci-worker-no-process-sandbox".into()),
            value_from: None,
        },
        EnvVar { name: "SCARAB_RUN".into(), value: Some(step.run.0.clone()), value_from: None },
        EnvVar { name: "SCARAB_STEP".into(), value: Some(step.step.0.clone()), value_from: None },
        EnvVar { name: "SCARAB_ATTEMPT".into(), value: Some(attempt.clone()), value_from: None },
    ];

    let container = Container {
        name: "build".to_string(),
        image: Some(BUILDKIT_IMAGE.to_string()),
        command: Some(vec!["buildctl-daemonless.sh".to_string()]),
        args: Some(build.buildctl_args()),
        env: Some(env),
        // Rootless: explicitly NOT privileged; non-root; unconfined seccomp so
        // the user-namespace worker can run (ADR-0018).
        security_context: Some(SecurityContext {
            privileged: Some(false),
            run_as_non_root: Some(true),
            run_as_user: Some(1000),
            seccomp_profile: Some(SeccompProfile {
                type_: "Unconfined".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    let labels = std::collections::BTreeMap::from([
        ("app.kubernetes.io/managed-by".to_string(), "scarab".to_string()),
        ("scarab.io/run".to_string(), sanitize_label(&step.run.0)),
        ("scarab.io/step".to_string(), sanitize_label(&step.step.0)),
        ("scarab.io/attempt".to_string(), sanitize_label(&attempt)),
    ]);
    // AppArmor unconfined for the build container (rootless buildkit).
    let annotations = std::collections::BTreeMap::from([(
        "container.apparmor.security.beta.kubernetes.io/build".to_string(),
        "unconfined".to_string(),
    )]);

    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            annotations: Some(annotations),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![container],
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The image an image-build step produced, recorded as an Artifact of record
/// (ADR-0018): the pushed reference and its content digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageArtifact {
    pub image: String,
    pub digest: String,
}

/// Record a built image's digest as an [`ImageArtifact`].
pub fn image_artifact(build: &BuildSpec, digest: &str) -> ImageArtifact {
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
/// is lifted from the container's terminated state.
pub fn pod_state(pod: &Pod) -> ExecState {
    let phase = pod
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("Pending");
    match phase {
        "Succeeded" => ExecState::Succeeded,
        "Failed" => ExecState::Failed {
            exit_code: container_exit_code(pod),
        },
        "Running" => ExecState::Running,
        // "Unknown" means the node stopped reporting — the backend lost it.
        "Unknown" => ExecState::Lost,
        _ => ExecState::Pending,
    }
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
        .or_else(|| pod.status.as_ref().unwrap().container_statuses.as_ref().unwrap().first())?
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
    use scarab_engine::{Attempt, AttemptId, RunId, StepId, StepStatus, Timestamp};

    fn step_with_attempt(run: &str, step: &str, attempt: &str) -> StepRun {
        StepRun {
            run: RunId(run.into()),
            step: StepId(step.into()),
            status: StepStatus::Running,
            attempts: vec![Attempt {
                id: AttemptId(attempt.into()),
                started_at: Timestamp(0),
                failure: None,
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
        }
    }

    #[test]
    fn step_pod_is_hardened_restricted_by_default() {
        let step = step_with_attempt("run-1", "build", "a1");
        let pod = build_pod("scarab-x", "scarab-run-1", &step, &busybox(), None);
        let sc = pod.spec.unwrap().containers[0]
            .security_context
            .clone()
            .expect("baseline security context must be set");
        // ADR-0039 restricted floor.
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(sc.run_as_user, None);
        assert_eq!(sc.privileged, Some(false));
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        assert_eq!(sc.capabilities.as_ref().unwrap().drop, Some(vec!["ALL".to_string()]));
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
        };
        let pod = build_pod("scarab-x", "scarab-run-1", &step, &spec, None);
        let sc = pod.spec.unwrap().containers[0].security_context.clone().unwrap();
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
        let pod = build_pod("scarab-x", "scarab-run-1", &step, &spec, None);
        let sc = pod.spec.unwrap().containers[0].security_context.clone().unwrap();
        assert_eq!(sc.run_as_non_root, Some(false));
        assert_eq!(sc.privileged, Some(false));
        // Self-service root stays unprivileged and non-escalating.
        assert_eq!(sc.allow_privilege_escalation, Some(false));
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
        let pod = build_pod("scarab-build-a1-deadbeef", "scarab-run-1", &step, &busybox(), None);

        let spec = pod.spec.unwrap();
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
        let c = &spec.containers[0];
        assert_eq!(c.image.as_deref(), Some("busybox:latest"));
        assert_eq!(
            c.command.as_ref().unwrap(),
            &vec!["echo".to_string(), "hi".to_string()]
        );

        let env = c.env.as_ref().unwrap();
        let get = |k: &str| env.iter().find(|e| e.name == k).and_then(|e| e.value.clone());
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
        let pod = build_pod("scarab-x", "scarab-run-1", &step, &busybox(), None);
        let spec = pod.spec.unwrap();
        assert!(spec.volumes.is_none(), "no shared volume without egress");
        assert!(spec.init_containers.is_none(), "no sidecar without egress");
        let env = spec.containers[0].env.as_ref().unwrap();
        assert!(!env.iter().any(|e| e.name == "SCARAB_RESULTS"), "no results env");
    }

    #[test]
    fn build_pod_with_egress_wires_shared_volume_and_a_native_sidecar() {
        let egress = ResultsEgress {
            base_url: "http://scarab-server".into(),
            token_secret: b"secret".to_vec(),
            sidecar_image: "ghcr.io/acme/scarab-sidecar:1".into(),
        };
        let step = step_with_attempt("run-1", "build", "a1");
        let pod = build_pod("scarab-x", "scarab-run-1", &step, &busybox(), Some(&egress));
        let spec = pod.spec.unwrap();

        // Shared results emptyDir.
        let vol = spec.volumes.as_ref().unwrap().iter().find(|v| v.name == RESULTS_VOLUME).unwrap();
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
            c.env.as_ref().unwrap().iter().find(|e| e.name == k).and_then(|e| e.value.clone())
        };
        assert_eq!(senv(stepc, "SCARAB_RESULTS").as_deref(), Some(RESULTS_MOUNT_PATH));

        // Native sidecar: initContainer, restartPolicy Always, fence token + URL,
        // read-only view of the results.
        let side = spec.init_containers.as_ref().unwrap().iter().find(|c| c.name == "scarab-results-egress").unwrap();
        assert_eq!(side.restart_policy.as_deref(), Some("Always"), "native sidecar");
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

    fn sample_build() -> BuildSpec {
        BuildSpec {
            context: "workspace".into(),
            dockerfile: "Dockerfile".into(),
            image: "registry.example/app:1.0".into(),
            push: true,
        }
    }

    #[test]
    fn build_pod_is_rootless_buildkit_and_not_privileged() {
        let step = step_with_attempt("run-1", "image", "a1");
        let pod = build_pod_for_build("scarab-image-a1", "scarab-run-1", &step, &sample_build());
        let c = &pod.spec.as_ref().unwrap().containers[0];

        assert_eq!(c.image.as_deref(), Some("moby/buildkit:rootless"));
        assert_eq!(c.command.as_ref().unwrap(), &vec!["buildctl-daemonless.sh".to_string()]);

        // Rootless security posture: never privileged, non-root, unconfined seccomp.
        let sc = c.security_context.as_ref().unwrap();
        assert_eq!(sc.privileged, Some(false));
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(sc.seccomp_profile.as_ref().unwrap().type_, "Unconfined");
        // AppArmor unconfined annotation for the build container.
        assert_eq!(
            pod.metadata
                .annotations
                .as_ref()
                .unwrap()
                .get("container.apparmor.security.beta.kubernetes.io/build")
                .map(String::as_str),
            Some("unconfined")
        );
    }

    #[test]
    fn build_args_carry_context_dockerfile_and_pushed_image() {
        let args = sample_build().buildctl_args().join(" ");
        assert!(args.contains("--frontend dockerfile.v0"));
        assert!(args.contains("context=workspace"));
        assert!(args.contains("filename=Dockerfile"));
        assert!(args.contains("type=image,name=registry.example/app:1.0,push=true"));
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
        assert_eq!(
            pod_state(&with_phase("Failed", Some(1))),
            ExecState::Failed { exit_code: Some(1) }
        );
        assert_eq!(pod_state(&with_phase("Unknown", None)), ExecState::Lost);
        // No status yet -> not scheduled -> Pending.
        assert_eq!(pod_state(&Pod::default()), ExecState::Pending);
    }
}
