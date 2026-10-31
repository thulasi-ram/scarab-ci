//! Live tier for the plugin runtime (ADR-0072 §9, §11): what only a real
//! cluster can show — that it ENFORCES the NetworkPolicy the unit tests only
//! shape, that Pod Security admits the workload, and that TokenReview vouches
//! for a projected broker token and for nothing else.
//!
//! `#[ignore]`d, so `cargo test` never runs it and never touches an ambient
//! kubeconfig. Asking for the ignored set is the opt-in, and a case then needs
//! `SCARAB_TEST_KUBE` as well: without it, it panics ([`enter_tier`]) rather
//! than passing having done nothing (git-bug dcc4ca9). Run it with `just
//! plugin-kube-tests` (or the whole tier with `just kube-tests`), which points
//! `KUBECONFIG` at the isolated proc-mode kind cluster.
//!
//! Once `SCARAB_TEST_KUBE` is set, a misconfiguration is a **panic**, never a
//! skip: a live case that silently does nothing reads as a green tier (the
//! `cluster.rs` lesson in `scarab-executor-k8s`). In particular the cluster
//! must come from an explicit `KUBECONFIG` whose current context is a kind
//! cluster — production contexts sit beside the local one, and this suite
//! creates and deletes namespaces.
//!
//! Everything runs in throwaway namespaces (`scarab-plugins-t-<nonce>` and a
//! fake broker's `scarab-broker-t-<nonce>`), deleted afterwards even when an
//! assertion fails. The fake broker is a busybox `httpd` serving
//! `/_scarab/plugin/v1/healthz` → `ok` in-cluster, so the tier needs no host
//! address and no Scarab server. Plugins have no DNS, so the runtime is
//! configured with the fake broker Service's ClusterIP (what the runtime
//! would resolve a name to in-cluster — the test process, on the host,
//! cannot resolve `*.svc`).

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use futures::FutureExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::authentication::v1::{TokenRequest, TokenRequestSpec};
use k8s_openapi::api::core::v1::{
    Container, Namespace, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, PodSpec,
    ProjectedVolumeSource, Service, ServiceAccount, ServiceAccountTokenProjection, ServicePort,
    ServiceSpec, Volume, VolumeMount, VolumeProjection, VolumeResourceRequirements,
};
use k8s_openapi::api::networking::v1::{NetworkPolicy, NetworkPolicySpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};

use scarab_plugin::{
    ConnectionSlot, InstallationState, PluginCapability, PluginInstallation, PluginManifest,
    PluginStorage, PluginTokenVerifier, RequestRule, RuntimeStatus, TokenRejected,
    VerifiedServiceAccount, PLUGIN_STORAGE_OWNER_ANNOTATION,
};
use scarab_plugin_k8s::objects::{
    restricted_container_security_context, restricted_pod_security_context, DEFAULT_DENY_POLICY,
    PLUGIN_EGRESS_POLICY,
};
use scarab_plugin_k8s::probe::{
    DEFAULT_PROBE_IMAGE, SELF_CHECK_CONTAINER, SELF_CHECK_DEADLINE_SECONDS,
};
use scarab_plugin_k8s::{
    BrokerPeer, K8sPluginRuntime, PluginRuntimeConfig, RuntimeError, TokenReviewVerifier,
    WorkloadState, LABEL_INSTALLATION,
};

const BROKER_PORT: u16 = 8090;
const INSTALLATION_ID: &str = "0123456789";

/// The gate every case passes first. The case is `#[ignore]`d, so reaching
/// this means someone asked for the ignored set: that is the opt-in. Without
/// `SCARAB_TEST_KUBE` as well, the case panics; it used to return and report
/// PASS (git-bug dcc4ca9).
fn enter_tier() {
    if std::env::var("SCARAB_TEST_KUBE").is_err() {
        panic!(
            "this live case was asked to run (`--ignored` is the opt-in) but \
             SCARAB_TEST_KUBE is not set, and it will not pass by doing nothing. Run \
             `just plugin-kube-tests`, which sets it and points KUBECONFIG at the \
             isolated proc-mode kind cluster."
        );
    }
}

/// Install `ring` as the process-level rustls CryptoProvider. This crate's
/// graph links more than one provider (`ring` and `aws-lc-rs`), so rustls
/// 0.23 refuses to auto-select and panics at the first TLS handshake — the
/// kube client dialing the API server. Same call as `scarab-server`'s
/// `main.rs`; the `Err` (a provider is already installed, by an earlier test
/// in this process) is ignored.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A client for the kind cluster the tier was pointed at — and only that.
/// Panics (the tier is opted in) on anything else. Every case builds its
/// client here, so this is where the CryptoProvider goes in.
async fn kind_client() -> Client {
    install_crypto_provider();
    let path = match std::env::var("KUBECONFIG") {
        Ok(p) if !p.is_empty() => p,
        _ => panic!(
            "KUBECONFIG is not set, but SCARAB_TEST_KUBE is — this tier never uses an ambient \
             kubeconfig. Run `just plugin-kube-tests`, which points it at the isolated \
             proc-mode kind cluster."
        ),
    };
    let kubeconfig =
        Kubeconfig::read_from(&path).unwrap_or_else(|e| panic!("reading KUBECONFIG {path}: {e}"));
    let context = kubeconfig.current_context.clone().unwrap_or_default();
    let allowed = std::env::var("SCARAB_TEST_KUBE_ALLOW_CONTEXT").unwrap_or_default();
    assert!(
        context.starts_with("kind-") || (!allowed.is_empty() && context == allowed),
        "KUBECONFIG {path} has current context `{context}`, which is not a kind cluster. This \
         suite creates and deletes namespaces; it runs only against kind, or against the one \
         context named in SCARAB_TEST_KUBE_ALLOW_CONTEXT."
    );
    let config = Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default())
        .await
        .unwrap_or_else(|e| panic!("loading context `{context}`: {e}"));
    Client::try_from(config).expect("kube client")
}

fn probe_image() -> String {
    std::env::var("SCARAB_TEST_PLUGIN_PROBE_IMAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PROBE_IMAGE.to_string())
}

/// The throwaway namespaces one case runs in.
struct Fixture {
    client: Client,
    plugins_ns: String,
    broker_ns: String,
    /// `http://<fake-broker ClusterIP>:<port>`.
    broker_url: String,
}

impl Fixture {
    fn runtime(&self) -> K8sPluginRuntime {
        K8sPluginRuntime::new(
            self.client.clone(),
            PluginRuntimeConfig {
                namespace: self.plugins_ns.clone(),
                broker_url: self.broker_url.clone(),
                broker_peer: Some(BrokerPeer::Pods {
                    namespace: self.broker_ns.clone(),
                    labels: BTreeMap::from([("app".to_string(), "fake-broker".to_string())]),
                }),
                broker_port: BROKER_PORT,
                probe_image: probe_image(),
            },
        )
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.plugins_ns)
    }

    /// The baseline, applied, and the broker URL as Pods dial it — what one
    /// reconcile pass starts with.
    async fn ready_runtime(&self) -> (K8sPluginRuntime, String) {
        let runtime = self.runtime();
        runtime.ensure_baseline().await.expect("baseline");
        let pod_broker_url = runtime.pod_broker_url().await.expect("broker URL");
        (runtime, pod_broker_url)
    }
}

fn namespace(name: &str, psa_enforce: &str) -> Namespace {
    Namespace {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(BTreeMap::from([
                (
                    "pod-security.kubernetes.io/enforce".to_string(),
                    psa_enforce.to_string(),
                ),
                ("scarab.io/test".to_string(), "plugin-k8s".to_string()),
            ])),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Run `body` against fresh namespaces (the plugin one under
/// `restricted`, plus the fake broker's), and delete them afterwards — on a
/// panic too, which is then re-raised.
async fn with_fixture<F, Fut>(body: F)
where
    F: FnOnce(Fixture) -> Fut,
    Fut: Future<Output = ()>,
{
    let client = kind_client().await;
    let nonce = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let plugins_ns = format!("scarab-plugins-t-{nonce}");
    let broker_ns = format!("scarab-broker-t-{nonce}");
    let namespaces: Api<Namespace> = Api::all(client.clone());
    namespaces
        .create(
            &PostParams::default(),
            &namespace(&plugins_ns, "restricted"),
        )
        .await
        .expect("create the plugin namespace");
    namespaces
        .create(&PostParams::default(), &namespace(&broker_ns, "baseline"))
        .await
        .expect("create the fake broker's namespace");
    let outcome = AssertUnwindSafe(async {
        let broker_ip = start_fake_broker(&client, &broker_ns).await;
        body(Fixture {
            client: client.clone(),
            plugins_ns: plugins_ns.clone(),
            broker_ns: broker_ns.clone(),
            broker_url: format!("http://{broker_ip}:{BROKER_PORT}"),
        })
        .await;
    })
    .catch_unwind()
    .await;
    for ns in [&plugins_ns, &broker_ns] {
        if let Err(e) = namespaces.delete(ns, &DeleteParams::background()).await {
            eprintln!("cleanup: deleting namespace {ns}: {e}");
        }
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// `fake-broker`: a busybox httpd answering `GET /_scarab/plugin/v1/healthz`
/// with `ok`, behind a ClusterIP Service on the broker port. Returns the
/// Service's ClusterIP.
async fn start_fake_broker(client: &Client, broker_ns: &str) -> String {
    let labels = BTreeMap::from([("app".to_string(), "fake-broker".to_string())]);
    let script = format!(
        "mkdir -p /tmp/w/_scarab/plugin/v1 && printf ok > /tmp/w/_scarab/plugin/v1/healthz && \
         exec httpd -f -p {BROKER_PORT} -h /tmp/w"
    );
    let pod = Pod {
        metadata: ObjectMeta {
            name: Some("fake-broker".to_string()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            automount_service_account_token: Some(false),
            containers: vec![Container {
                name: "httpd".to_string(),
                image: Some(probe_image()),
                command: Some(vec!["sh".to_string(), "-c".to_string(), script]),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let pods: Api<Pod> = Api::namespaced(client.clone(), broker_ns);
    pods.create(&PostParams::default(), &pod)
        .await
        .expect("create the fake broker");
    let service = Service {
        metadata: ObjectMeta {
            name: Some("fake-broker".to_string()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(labels),
            ports: Some(vec![ServicePort {
                port: i32::from(BROKER_PORT),
                target_port: Some(IntOrString::Int(i32::from(BROKER_PORT))),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let created = Api::<Service>::namespaced(client.clone(), broker_ns)
        .create(&PostParams::default(), &service)
        .await
        .expect("create the fake broker's Service");
    wait_running(&pods, "fake-broker").await;
    created
        .spec
        .and_then(|s| s.cluster_ip)
        .filter(|ip| !ip.is_empty() && ip != "None")
        .expect("the fake broker's Service has a ClusterIP")
}

async fn wait_running(pods: &Api<Pod>, name: &str) {
    for _ in 0..120 {
        let pod = pods.get(name).await.expect("read Pod");
        if pod.status.and_then(|s| s.phase).as_deref() == Some("Running") {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("Pod {name} was not Running within 120s");
}

/// The isolation probe passes on a cluster that enforces NetworkPolicy
/// (kind ≥ 0.24's kindnet does): the plugin-labelled Pod reaches the broker,
/// and nothing else — no DNS answer either — while the control Pod reaches
/// the canary and gets one. kindnet programs a new Pod's policy a few
/// seconds after it starts; the probe's first passes see it open, and this
/// is the case that proves the probe waits that out rather than failing.
#[tokio::test]
#[ignore]
async fn the_isolation_probe_passes_on_this_cluster() {
    enter_tier();
    with_fixture(|f| async move {
        let (runtime, pod_broker_url) = f.ready_runtime().await;
        let report = runtime.probe_isolation(&pod_broker_url).await;
        assert!(report.passed(), "{}", report.message());
        let leftovers = f
            .pods()
            .list(&ListParams::default().labels("scarab.io/isolation-probe"))
            .await
            .unwrap();
        assert!(
            leftovers
                .items
                .iter()
                .all(|p| p.metadata.deletion_timestamp.is_some()),
            "the probe deletes its Pods"
        );
    })
    .await;
}

/// The probe's negative case, for real: with the egress policies gone a
/// plugin-labelled Pod reaches the canary, and the probe FAILS — saying
/// NetworkPolicy is not enforced, rather than passing an open namespace.
#[tokio::test]
#[ignore]
async fn without_the_egress_policies_the_canary_is_reachable_and_the_probe_fails() {
    enter_tier();
    with_fixture(|f| async move {
        let (runtime, pod_broker_url) = f.ready_runtime().await;
        remove_plugin_policies(&f).await;
        let report = runtime.probe_isolation(&pod_broker_url).await;
        assert!(!report.passed(), "an open namespace must fail the probe");
        assert!(
            report.message().contains("NOT enforced"),
            "the canary must be what failed it: {}",
            report.message()
        );
        // The baseline puts them back.
        runtime.ensure_baseline().await.expect("baseline restored");
    })
    .await;
}

/// Fail closed on the namespace itself: a level below `restricted`, or a
/// NetworkPolicy the runtime did not write, refuses the baseline.
#[tokio::test]
#[ignore]
async fn the_baseline_refuses_a_namespace_it_cannot_vouch_for() {
    enter_tier();
    with_fixture(|f| async move {
        // A stray allow-all egress policy in the plugin namespace.
        let stray = NetworkPolicy {
            metadata: ObjectMeta {
                name: Some("allow-everything".to_string()),
                ..Default::default()
            },
            spec: Some(NetworkPolicySpec {
                pod_selector: Some(LabelSelector::default()),
                policy_types: Some(vec!["Egress".to_string()]),
                egress: Some(vec![Default::default()]),
                ingress: None,
            }),
        };
        let policies: Api<NetworkPolicy> = Api::namespaced(f.client.clone(), &f.plugins_ns);
        policies
            .create(&PostParams::default(), &stray)
            .await
            .unwrap();
        let err = f.runtime().ensure_baseline().await.unwrap_err();
        assert!(
            matches!(&err, RuntimeError::BaselineRefused(m) if m.contains("allow-everything")),
            "{err}"
        );

        // The fake broker's namespace is only `baseline`.
        let mut config = f.runtime().config().clone();
        config.namespace = f.broker_ns.clone();
        let err = K8sPluginRuntime::new(f.client.clone(), config)
            .ensure_baseline()
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RuntimeError::BaselineRefused(m) if m.contains("restricted")),
            "{err}"
        );
    })
    .await;
}

/// Open the namespace: delete `default-deny` and `plugin-egress`, so a
/// plugin-labelled Pod reaches everything.
async fn remove_plugin_policies(f: &Fixture) {
    let policies: Api<NetworkPolicy> = Api::namespaced(f.client.clone(), &f.plugins_ns);
    for name in [DEFAULT_DENY_POLICY, PLUGIN_EGRESS_POLICY] {
        policies
            .delete(name, &DeleteParams::default())
            .await
            .expect("delete a baseline policy");
    }
}

fn installation(image: &str) -> PluginInstallation {
    PluginInstallation {
        id: INSTALLATION_ID.into(),
        org: "acme".into(),
        name: "argocd-prod".into(),
        plugin: "argocd".into(),
        image: image.into(),
        // A manifest `validate()` accepts: `use_connection` needs a slot.
        manifest: PluginManifest {
            plugin: "argocd".into(),
            capabilities: vec![PluginCapability::UseConnection],
            connections: vec![ConnectionSlot {
                name: "argocd".into(),
                auth_schemes: vec!["bearer".into()],
                read_rules: vec![RequestRule {
                    method: "GET".into(),
                    path: "/api/version".into(),
                    query: vec![],
                }],
            }],
            config_keys: vec![],
            gates: Default::default(),
        },
        manifest_sha256: "f".repeat(64),
        config: BTreeMap::new(),
        state: InstallationState::Enabled,
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

/// The workload against a real API server: Pod Security `restricted` admits
/// the Pod the Deployment makes, isolation self-check initContainer and all;
/// on this enforcing cluster the self-check PASSES (broker by IP reached,
/// nothing else); stop scales it to zero and deletes the ServiceAccount;
/// uninstall removes the rest. (The Plugin image is the probe's busybox,
/// which exits at once — admission and the self-check are under test, not
/// the Plugin.)
#[tokio::test]
#[ignore]
async fn a_workload_is_admitted_stopped_and_uninstalled() {
    enter_tier();
    with_fixture(|f| async move {
        let (runtime, pod_broker_url) = f.ready_runtime().await;
        runtime
            .ensure(&installation(&probe_image()), &pod_broker_url)
            .await
            .expect("ensure");
        // Applying twice changes nothing.
        runtime
            .ensure(&installation(&probe_image()), &pod_broker_url)
            .await
            .expect("ensure again");
        let selector = format!("{LABEL_INSTALLATION}={INSTALLATION_ID}");
        let mut admitted = None;
        for _ in 0..60 {
            let pods = f
                .pods()
                .list(&ListParams::default().labels(&selector))
                .await
                .unwrap();
            if let Some(pod) = pods.items.into_iter().next() {
                admitted = Some(pod);
                break;
            }
            if let WorkloadState::Failing(why) = runtime
                .workload_state(&installation(&probe_image()))
                .await
                .unwrap()
            {
                if why.contains("PodSecurity") {
                    panic!("restricted Pod Security refused the plugin Pod: {why}");
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let pod = admitted.expect("the Deployment made a Pod within 60s");
        let pod_name = pod.metadata.name.clone().unwrap();
        let self_check_exit = wait_self_check_exit(&f.pods(), &pod_name).await;
        assert_eq!(
            self_check_exit,
            Some(0),
            "on an enforcing cluster the isolation self-check passes"
        );
        let spec = pod.spec.unwrap();
        assert_eq!(spec.automount_service_account_token, Some(false));
        assert_eq!(
            spec.service_account_name.as_deref(),
            Some("plugin-0123456789")
        );
        assert_eq!(
            runtime
                .workload_ids()
                .await
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![INSTALLATION_ID.to_string()]
        );

        runtime.stop(INSTALLATION_ID).await.expect("stop");
        assert_eq!(
            runtime
                .workload_state(&installation(&probe_image()))
                .await
                .unwrap(),
            WorkloadState::Stopped
        );
        let sas: Api<ServiceAccount> = Api::namespaced(f.client.clone(), &f.plugins_ns);
        assert!(sas.get_opt("plugin-0123456789").await.unwrap().is_none());
        runtime
            .stop(INSTALLATION_ID)
            .await
            .expect("stop is idempotent");

        runtime.uninstall(INSTALLATION_ID).await.expect("uninstall");
        assert_eq!(
            runtime
                .workload_state(&installation(&probe_image()))
                .await
                .unwrap(),
            WorkloadState::Absent
        );
        runtime
            .uninstall(INSTALLATION_ID)
            .await
            .expect("uninstall is idempotent");
    })
    .await;
}

/// The first exit code of `pod_name`'s isolation self-check, waiting up to
/// its deadline plus slack; `None` if it never finished.
async fn wait_self_check_exit(pods: &Api<Pod>, pod_name: &str) -> Option<i32> {
    let wait = SELF_CHECK_DEADLINE_SECONDS + 60;
    for _ in 0..wait {
        let pod = pods.get(pod_name).await.expect("read the plugin Pod");
        let statuses = pod
            .status
            .and_then(|s| s.init_container_statuses)
            .unwrap_or_default();
        for st in statuses
            .into_iter()
            .filter(|s| s.name == SELF_CHECK_CONTAINER)
        {
            let terminated = st
                .state
                .and_then(|s| s.terminated)
                .or_else(|| st.last_state.and_then(|s| s.terminated));
            if let Some(t) = terminated {
                return Some(t.exit_code);
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    None
}

/// The self-check is what proves the Plugin's OWN Pod: in a namespace whose
/// policies are gone, it must keep the Plugin from starting — and the
/// runtime must say why — rather than run it unprotected.
#[tokio::test]
#[ignore]
async fn in_an_open_namespace_the_self_check_keeps_the_plugin_from_starting() {
    enter_tier();
    with_fixture(|f| async move {
        let (runtime, pod_broker_url) = f.ready_runtime().await;
        remove_plugin_policies(&f).await;
        runtime
            .ensure(&installation(&probe_image()), &pod_broker_url)
            .await
            .expect("ensure");
        let selector = format!("{LABEL_INSTALLATION}={INSTALLATION_ID}");
        let mut state = WorkloadState::Absent;
        // The check retries until its deadline, then runs one last pass (up
        // to ~35 s), and the runtime reports it from that first failed run;
        // plus the Pod's own start, and margin. Wall-clock, not a count of
        // polls: each poll is several API calls.
        let wait = Duration::from_secs(u64::from(SELF_CHECK_DEADLINE_SECONDS) + 120);
        let started = Instant::now();
        while started.elapsed() < wait {
            state = runtime
                .workload_state(&installation(&probe_image()))
                .await
                .unwrap();
            if matches!(&state, WorkloadState::Failing(why) if why.contains("self-check")) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let WorkloadState::Failing(why) = &state else {
            panic!(
                "the self-check never failed the workload in an open namespace within {}s: \
                 {state:?}",
                wait.as_secs()
            );
        };
        assert!(why.contains("isolation self-check failed"), "{why}");
        // The operator is told WHAT was open: the check's last pass.
        assert!(
            why.contains("SCARAB_PROBE") && why.contains("reached"),
            "the self-check's verdict must be in the message: {why}"
        );
        // The Plugin's own container never ran.
        let pods = f
            .pods()
            .list(&ListParams::default().labels(&selector))
            .await
            .unwrap();
        for pod in pods.items {
            let started = pod
                .status
                .and_then(|s| s.container_statuses)
                .unwrap_or_default()
                .iter()
                .any(|c| c.state.as_ref().is_some_and(|s| s.waiting.is_none()));
            assert!(
                !started,
                "the Plugin container started past a failing self-check"
            );
        }
    })
    .await;
}

fn projected_token(name: &str, audience: &str) -> Volume {
    Volume {
        name: name.to_string(),
        projected: Some(ProjectedVolumeSource {
            sources: Some(vec![VolumeProjection {
                service_account_token: Some(ServiceAccountTokenProjection {
                    audience: Some(audience.to_string()),
                    expiration_seconds: Some(600),
                    path: "token".to_string(),
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A Pod running as `sa_name` that prints two projected tokens — one for
/// `scarab-broker`, one for another audience — and then waits. Returns
/// `(broker_token, other_token)`.
async fn start_token_holder(f: &Fixture, name: &str, sa_name: &str) -> (String, String) {
    let holder = Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            service_account_name: Some(sa_name.to_string()),
            automount_service_account_token: Some(false),
            termination_grace_period_seconds: Some(0),
            security_context: Some(restricted_pod_security_context()),
            containers: vec![Container {
                name: "holder".to_string(),
                image: Some(probe_image()),
                command: Some(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "cat /var/run/secrets/broker/token; echo; \
                     cat /var/run/secrets/other/token; echo; sleep 600"
                        .to_string(),
                ]),
                security_context: Some(restricted_container_security_context()),
                volume_mounts: Some(vec![
                    VolumeMount {
                        name: "broker".to_string(),
                        mount_path: "/var/run/secrets/broker".to_string(),
                        read_only: Some(true),
                        ..Default::default()
                    },
                    VolumeMount {
                        name: "other".to_string(),
                        mount_path: "/var/run/secrets/other".to_string(),
                        read_only: Some(true),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }],
            volumes: Some(vec![
                projected_token("broker", "scarab-broker"),
                projected_token("other", "not-the-broker"),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pods = f.pods();
    pods.create(&PostParams::default(), &holder).await.unwrap();
    wait_running(&pods, name).await;
    let mut tokens = Vec::new();
    for _ in 0..30 {
        let log = pods
            .logs(name, &LogParams::default())
            .await
            .unwrap_or_default();
        tokens = log
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if tokens.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(tokens.len(), 2, "{name} printed both tokens");
    (tokens[0].clone(), tokens[1].clone())
}

/// How long revoking a token (deleting its Pod, or its ServiceAccount) may
/// take to show at TokenReview: kube-apiserver caches a SUCCESSFUL token
/// authentication itself (10 s by default) — the TokenReview API goes
/// through that cache too — so a review right after the revocation can
/// still say yes. Plus margin for its informers to see the deletion.
const REVOCATION_VISIBLE_WITHIN: Duration = Duration::from_secs(30);

/// TokenReview's verdict on `token`, asked until it is a rejection or
/// [`REVOCATION_VISIBLE_WITHIN`] passes; the last verdict. A fresh verifier
/// for every ask, so OUR verdict cache never answers for the API server.
async fn verdict_once_revoked(
    client: &Client,
    token: &str,
) -> Result<VerifiedServiceAccount, TokenRejected> {
    let started = Instant::now();
    loop {
        let verdict = TokenReviewVerifier::new(client.clone()).verify(token).await;
        if verdict.is_err() || started.elapsed() >= REVOCATION_VISIBLE_WITHIN {
            return verdict;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// TokenReview, for real: a projected `scarab-broker` token verifies as its
/// ServiceAccount; a token for another audience does not; a TokenRequest
/// with no bound Pod does not; once its Pod is gone, neither does the good
/// token; and — the kill switch at the source — once the runtime's `stop`
/// has deleted the ServiceAccount, a token whose Pod is STILL RUNNING is
/// rejected too.
#[tokio::test]
#[ignore]
async fn token_review_accepts_only_a_live_pods_broker_token() {
    enter_tier();
    with_fixture(|f| async move {
        // The installation's ServiceAccount, by the runtime's own naming.
        let sa_name = "plugin-0123456789";
        let sas: Api<ServiceAccount> = Api::namespaced(f.client.clone(), &f.plugins_ns);
        sas.create(
            &PostParams::default(),
            &ServiceAccount {
                metadata: ObjectMeta {
                    name: Some(sa_name.to_string()),
                    ..Default::default()
                },
                automount_service_account_token: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let (broker_token, other_token) = start_token_holder(&f, "token-holder", sa_name).await;
        let (survivor_token, _) = start_token_holder(&f, "token-survivor", sa_name).await;

        let verifier = TokenReviewVerifier::new(f.client.clone());
        let as_the_installation = Ok(VerifiedServiceAccount {
            namespace: f.plugins_ns.clone(),
            service_account: sa_name.to_string(),
        });
        assert_eq!(verifier.verify(&broker_token).await, as_the_installation);
        assert!(
            verifier.verify(&other_token).await.is_err(),
            "a token for another audience must not verify"
        );
        let unbound = sas
            .create_token_request(
                sa_name,
                &PostParams::default(),
                &TokenRequest {
                    spec: Some(TokenRequestSpec {
                        audiences: Some(vec!["scarab-broker".to_string()]),
                        expiration_seconds: Some(600),
                        bound_object_ref: None,
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .status
            .and_then(|status| status.token)
            .expect("a minted token");
        let err = verifier.verify(&unbound).await.unwrap_err();
        assert!(err.0.contains("not bound to a Pod"), "{err}");

        // The bound object: with its Pod gone — the object itself, not
        // merely terminating — the good token is nothing (once the API
        // server's own success cache has let go of it).
        let pods = f.pods();
        pods.delete("token-holder", &DeleteParams::default().grace_period(0))
            .await
            .unwrap();
        let mut gone = false;
        for _ in 0..60 {
            if pods.get_opt("token-holder").await.unwrap().is_none() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert!(gone, "the token holder's Pod was not deleted within 60s");
        let verdict = verdict_once_revoked(&f.client, &broker_token).await;
        assert!(
            verdict.is_err(),
            "a token must die with its Pod (still accepted {}s after it was gone): {verdict:?}",
            REVOCATION_VISIBLE_WITHIN.as_secs()
        );

        // The kill switch at the source: `stop` deletes the ServiceAccount,
        // and a token whose Pod is still running dies with it.
        assert_eq!(
            TokenReviewVerifier::new(f.client.clone())
                .verify(&survivor_token)
                .await,
            as_the_installation,
            "the survivor's token is good before the stop"
        );
        f.runtime().stop(INSTALLATION_ID).await.expect("stop");
        assert!(sas.get_opt(sa_name).await.unwrap().is_none());
        assert!(
            pods.get_opt("token-survivor").await.unwrap().is_some(),
            "the Pod still exists, so only the ServiceAccount can be what revoked the token"
        );
        // The review just above primed the API server's success cache, so
        // it may take that long to say no.
        let err = verdict_once_revoked(&f.client, &survivor_token)
            .await
            .expect_err("a token must die with its ServiceAccount");
        assert!(
            err.0.contains("not authenticated"),
            "a token must die with its ServiceAccount: {err}"
        );
        assert!(
            pods.get_opt("token-survivor").await.unwrap().is_some(),
            "the Pod outlived the revocation, so it was the ServiceAccount's deletion"
        );
    })
    .await;
}

/// ADR-0074 §7 on a real cluster: the runtime refuses a storage claim that
/// is missing or not annotated for the installation (holding the workload,
/// with the claim named), and mounts an annotated one; Pod Security
/// `restricted` admits the Pod with the claim; and the Plugin's own Pod
/// spec (the runtime's template, only its command replaced) can write to
/// it as uid 65532. kind's default StorageClass binds on first use, so the
/// claim binds only once a Pod is scheduled.
#[tokio::test]
#[ignore]
async fn a_plugin_pod_mounts_its_annotated_claim_and_can_write_to_it() {
    enter_tier();
    with_fixture(|f| async move {
        let (runtime, pod_broker_url) = f.ready_runtime().await;
        let mut inst = installation(&probe_image());
        inst.storage = Some(PluginStorage::new("plugin-state", None).unwrap());
        let claims: Api<PersistentVolumeClaim> = Api::namespaced(f.client.clone(), &f.plugins_ns);

        // Missing: refused, naming the claim; nothing is created.
        let err = runtime.ensure(&inst, &pod_broker_url).await.unwrap_err();
        assert!(
            matches!(&err, RuntimeError::Storage(m) if m.contains("`plugin-state` does not exist")),
            "{err}"
        );
        assert_eq!(
            runtime.workload_state(&inst).await.unwrap(),
            WorkloadState::Absent
        );

        // Present but not annotated for this installation: refused.
        let mut claim = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("plugin-state".to_string()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                resources: Some(VolumeResourceRequirements {
                    requests: Some(BTreeMap::from([(
                        "storage".to_string(),
                        Quantity("64Mi".to_string()),
                    )])),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        claims
            .create(&PostParams::default(), &claim)
            .await
            .expect("create the claim");
        let err = runtime.ensure(&inst, &pod_broker_url).await.unwrap_err();
        assert!(
            matches!(&err, RuntimeError::Storage(m) if m.contains("acme/argocd-prod")),
            "{err}"
        );

        // Annotated by the operator: mounted.
        claim.metadata.annotations = Some(BTreeMap::from([(
            PLUGIN_STORAGE_OWNER_ANNOTATION.to_string(),
            "acme/argocd-prod".to_string(),
        )]));
        claims
            .patch(
                "plugin-state",
                &PatchParams::default(),
                &Patch::Merge(&serde_json::json!({
                    "metadata": { "annotations": claim.metadata.annotations }
                })),
            )
            .await
            .expect("annotate the claim");
        runtime
            .ensure(&inst, &pod_broker_url)
            .await
            .expect("ensure with an annotated claim");
        let deployment = Api::<Deployment>::namespaced(f.client.clone(), &f.plugins_ns)
            .get("plugin-0123456789")
            .await
            .expect("the Deployment");
        let template = deployment.spec.unwrap().template;
        assert!(
            template
                .spec
                .as_ref()
                .unwrap()
                .volumes
                .iter()
                .flatten()
                .any(|v| v
                    .persistent_volume_claim
                    .as_ref()
                    .is_some_and(|c| c.claim_name == "plugin-state")),
            "the applied Deployment mounts the claim"
        );

        // The runtime's own Pod spec, with a command that writes to the claim
        // and reads it back, as the Plugin's uid. The Deployment is scaled to
        // zero first (its ServiceAccount kept), so the writer is the claim's
        // only user.
        Api::<Deployment>::namespaced(f.client.clone(), &f.plugins_ns)
            .patch(
                "plugin-0123456789",
                &PatchParams::default(),
                &Patch::Merge(&serde_json::json!({ "spec": { "replicas": 0 } })),
            )
            .await
            .expect("scale the plugin to zero");
        let mut spec = template.spec.unwrap();
        spec.restart_policy = Some("Never".to_string());
        spec.containers[0].command = Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "set -e; id -u; echo written-by-plugin > /data/proof; cat /data/proof".to_string(),
        ]);
        let writer = Pod {
            metadata: ObjectMeta {
                name: Some("storage-writer".to_string()),
                labels: template.metadata.and_then(|m| m.labels),
                ..Default::default()
            },
            spec: Some(spec),
            ..Default::default()
        };
        let pods = f.pods();
        pods.create(&PostParams::default(), &writer)
            .await
            .expect("Pod Security `restricted` admits the Plugin Pod with its claim");
        let wait = Duration::from_secs(u64::from(SELF_CHECK_DEADLINE_SECONDS) + 180);
        let started = Instant::now();
        let phase = loop {
            let phase = pods
                .get("storage-writer")
                .await
                .unwrap()
                .status
                .and_then(|s| s.phase)
                .unwrap_or_default();
            if phase == "Succeeded" || phase == "Failed" || started.elapsed() > wait {
                break phase;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        };
        let log = pods
            .logs(
                "storage-writer",
                &LogParams {
                    container: Some("plugin".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_default();
        assert_eq!(phase, "Succeeded", "the writer did not succeed: {log}");
        assert!(log.contains("65532"), "it ran as the Plugin's uid: {log}");
        assert!(log.contains("written-by-plugin"), "{log}");
        let bound = claims.get("plugin-state").await.unwrap();
        assert_eq!(bound.status.and_then(|s| s.phase).as_deref(), Some("Bound"));
    })
    .await;
}
