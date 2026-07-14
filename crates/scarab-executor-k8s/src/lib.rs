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
use k8s_openapi::api::core::v1::{
    Capabilities, Container, EnvVar, Pod, PodSpec, SeccompProfile, SecurityContext,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, PostParams};

use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{ExecError, Executor, StepRun, StepSpec};

/// Default graceful-cancel window: SIGTERM, then SIGKILL after this many seconds.
const CANCEL_GRACE_SECS: i64 = 30;

/// A Kubernetes-backed executor. Holds an optional client so the composition
/// root can construct it without contacting an API server.
pub struct K8sExecutor {
    client: Option<kube::Client>,
    namespace: String,
}

impl K8sExecutor {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            client: None,
            namespace: namespace.into(),
        }
    }

    pub fn with_client(namespace: impl Into<String>, client: kube::Client) -> Self {
        Self {
            client: Some(client),
            namespace: namespace.into(),
        }
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

        let pod = build_pod(&name, &self.namespace, step, spec);
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

    // `results` (ADR-0040) uses the port default (empty) for now. A
    // `restartPolicy: Never` Pod is torn down after completion, so its
    // `/scarab/results/*.json` cannot be scraped post-hoc; capturing them needs
    // the emit channel to stream results out before teardown (the injected
    // `scarab` CLI → log/result sidecar, ADR-0008). Tracked as a follow-up — the
    // local backend already captures results end-to-end.
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

/// Build the bare Pod for a step: one container running `spec`, restartPolicy
/// Never, with the fence injected as env vars for cooperating idempotency
/// (ADR-0021) and labels for observability.
pub fn build_pod(name: &str, namespace: &str, step: &StepRun, spec: &StepSpec) -> Pod {
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

    let container = Container {
        name: "step".to_string(),
        image: Some(spec.image.clone()),
        command: (!spec.command.is_empty()).then(|| spec.command.clone()),
        env: Some(env),
        security_context: Some(step_security_context(spec)),
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

    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
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
    pod.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .first()?
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
        let pod = build_pod("scarab-x", "scarab-run-1", &step, &busybox());
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
        let pod = build_pod("scarab-x", "scarab-run-1", &step, &spec);
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
        let pod = build_pod("scarab-x", "scarab-run-1", &step, &spec);
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
        let pod = build_pod("scarab-build-a1-deadbeef", "scarab-run-1", &step, &busybox());

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
