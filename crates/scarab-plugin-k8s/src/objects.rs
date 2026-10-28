//! The Kubernetes objects of the plugin runtime, built as data (ADR-0072 §9).
//!
//! Pure: no client, no I/O. What a Plugin Pod may do is decided here, so it
//! is unit-tested here — the live tier only proves the cluster enforces it.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, EmptyDirVolumeSource, EnvVar, PersistentVolumeClaim,
    PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec, PodTemplateSpec,
    ProjectedVolumeSource, ResourceRequirements, SeccompProfile, SecurityContext, ServiceAccount,
    ServiceAccountTokenProjection, Volume, VolumeMount, VolumeProjection,
};
use k8s_openapi::api::networking::v1::{
    IPBlock, NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyIngressRule, NetworkPolicyPeer,
    NetworkPolicyPort, NetworkPolicySpec,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use scarab_plugin::{
    service_account_for, PluginInstallation, PluginStorage, BROKER_TOKEN_AUDIENCE,
    PLUGIN_ENV_BROKER_TOKEN_FILE, PLUGIN_ENV_BROKER_URL, PLUGIN_STORAGE_OWNER_ANNOTATION,
    RESERVED_CONFIG_KEY_PREFIX,
};

use crate::{BrokerPeer, LABEL_INSTALLATION, LABEL_MANAGED_BY, LABEL_PLUGIN_WORKLOAD, MANAGED_BY};

/// The baseline's NetworkPolicy names.
pub const DEFAULT_DENY_POLICY: &str = "default-deny";
pub const PLUGIN_EGRESS_POLICY: &str = "plugin-egress";
/// The isolation probe's two allowances (see [`crate::probe`]): its control
/// Pod may reach the canary, and the canary accepts connections from the
/// namespace. Neither selects a Plugin workload.
pub const PROBE_CONTROL_EGRESS_POLICY: &str = "isolation-probe-control-egress";
pub const PROBE_CANARY_INGRESS_POLICY: &str = "isolation-probe-canary-ingress";

/// The isolation probe's Pods: `scarab.io/isolation-probe=<nonce>` on all
/// three, and a role.
pub const LABEL_PROBE: &str = "scarab.io/isolation-probe";
pub const LABEL_PROBE_ROLE: &str = "scarab.io/isolation-probe-role";
pub const PROBE_ROLE_CANARY: &str = "canary";
pub const PROBE_ROLE_CONTROL: &str = "control";
pub const PROBE_ROLE_PLUGIN: &str = "plugin";
/// The port the canary listens on.
pub const PROBE_CANARY_PORT: u16 = 8080;

/// Where the cluster's DNS lives. Every mainstream distribution (kind, k3s,
/// EKS, GKE, AKS) runs CoreDNS/kube-dns in `kube-system` labelled
/// `k8s-app=kube-dns`. Only the probe's CONTROL Pod is allowed to it — its
/// positive DNS check is what makes a Plugin's "no answer" mean something; a
/// cluster that resolves elsewhere (node-local DNS) fails that check —
/// loudly, which is the point. A Plugin gets no DNS at all.
pub const DNS_NAMESPACE: &str = "kube-system";
pub const DNS_POD_LABEL: (&str, &str) = ("k8s-app", "kube-dns");

/// The projected broker token (ADR-0072 §11): short-lived — the kubelet
/// rotates it in place before it expires — and good only for the broker.
pub const BROKER_TOKEN_EXPIRATION_SECONDS: i64 = 600;
pub const BROKER_TOKEN_DIR: &str = "/var/run/secrets/scarab/broker";
pub const BROKER_TOKEN_FILE: &str = "token";

/// The unprivileged uid/gid a Plugin runs as (distroless `nonroot`).
pub const PLUGIN_UID: i64 = 65532;

/// The scratch directory every Plugin gets: the one writable path beside
/// its storage claim, if it has one.
pub const PLUGIN_TMP_DIR: &str = "/tmp";

/// The Pod volume an installation's storage claim is mounted from.
pub const STORAGE_VOLUME: &str = "storage";

/// The CPU ceiling of every container in a Plugin Pod: a spinning (or
/// compromised) Plugin gets half a core, never the node.
pub const PLUGIN_CPU_LIMIT: &str = "500m";

/// The name of an installation's ServiceAccount AND Deployment:
/// `plugin-<id>`. Keyed by id (never the name), so a reinstall under a new id
/// cannot be reached with the old installation's tokens.
pub fn workload_name(installation_id: &str) -> String {
    service_account_for(installation_id)
}

/// The labels every object of one installation carries; the Deployment's
/// selector is the id label alone.
pub fn installation_labels(installation_id: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_INSTALLATION.to_string(), installation_id.to_string()),
        (LABEL_MANAGED_BY.to_string(), MANAGED_BY.to_string()),
    ])
}

fn meta(name: &str, namespace: &str, labels: BTreeMap<String, String>) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: Some(namespace.to_string()),
        labels: Some(labels),
        ..Default::default()
    }
}

fn managed_labels() -> BTreeMap<String, String> {
    BTreeMap::from([(LABEL_MANAGED_BY.to_string(), MANAGED_BY.to_string())])
}

fn selector(labels: &[(&str, &str)]) -> LabelSelector {
    LabelSelector {
        match_labels: Some(
            labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ),
        ..Default::default()
    }
}

fn tcp(port: u16) -> NetworkPolicyPort {
    NetworkPolicyPort {
        port: Some(IntOrString::Int(i32::from(port))),
        protocol: Some("TCP".to_string()),
        ..Default::default()
    }
}

/// `default-deny`: every Pod in the namespace, ingress AND egress, no rule —
/// so Plugin to Plugin is denied both ways, and a Pod with no other policy
/// reaches nothing (ADR-0072 §9).
pub fn default_deny_policy(namespace: &str) -> NetworkPolicy {
    NetworkPolicy {
        metadata: meta(DEFAULT_DENY_POLICY, namespace, managed_labels()),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(LabelSelector::default()),
            policy_types: Some(vec!["Ingress".to_string(), "Egress".to_string()]),
            ingress: None,
            egress: None,
        }),
    }
}

/// The DNS egress rule: UDP and TCP 53 to the cluster DNS Pods only. For the
/// probe's control Pod alone — never a Plugin: the cluster resolver forwards
/// any name upstream, so even "kube-dns only" is a tunnel out.
fn dns_egress_rule() -> NetworkPolicyEgressRule {
    NetworkPolicyEgressRule {
        to: Some(vec![NetworkPolicyPeer {
            namespace_selector: Some(selector(&[("kubernetes.io/metadata.name", DNS_NAMESPACE)])),
            pod_selector: Some(selector(&[DNS_POD_LABEL])),
            ..Default::default()
        }]),
        ports: Some(vec![
            NetworkPolicyPort {
                port: Some(IntOrString::Int(53)),
                protocol: Some("UDP".to_string()),
                ..Default::default()
            },
            tcp(53),
        ]),
    }
}

/// `plugin-egress`: what a Plugin workload may reach — the CredentialBroker's
/// port, nothing else (ADR-0072 §9). Not even DNS: CoreDNS's default
/// `forward .` resolves `<data>.attacker.example` recursively, so a DNS
/// allowance is a two-way channel out (data in the query names, commands in
/// the answers). A Plugin needs none — its only peer is the broker, and it is
/// handed the broker by IP. Egress only: its ingress stays at
/// `default-deny`'s nothing (owner default 4).
pub fn plugin_egress_policy(
    namespace: &str,
    broker: &BrokerPeer,
    broker_port: u16,
) -> NetworkPolicy {
    let broker_peer = match broker {
        BrokerPeer::Pods {
            namespace: broker_ns,
            labels,
        } => NetworkPolicyPeer {
            namespace_selector: Some(selector(&[(
                "kubernetes.io/metadata.name",
                broker_ns.as_str(),
            )])),
            pod_selector: Some(LabelSelector {
                match_labels: Some(labels.clone()),
                ..Default::default()
            }),
            ..Default::default()
        },
        BrokerPeer::Cidr(cidr) => NetworkPolicyPeer {
            ip_block: Some(IPBlock {
                cidr: cidr.clone(),
                except: None,
            }),
            ..Default::default()
        },
    };
    NetworkPolicy {
        metadata: meta(PLUGIN_EGRESS_POLICY, namespace, managed_labels()),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(selector(&[(LABEL_PLUGIN_WORKLOAD, "true")])),
            policy_types: Some(vec!["Egress".to_string()]),
            ingress: None,
            egress: Some(vec![NetworkPolicyEgressRule {
                to: Some(vec![broker_peer]),
                ports: Some(vec![tcp(broker_port)]),
            }]),
        }),
    }
}

/// The probe's control Pod may reach the canary and the cluster DNS. This is
/// what makes a failed plugin-side connection or lookup mean "the policy
/// held" rather than "the canary / the resolver was never reachable".
pub fn probe_control_egress_policy(namespace: &str) -> NetworkPolicy {
    NetworkPolicy {
        metadata: meta(PROBE_CONTROL_EGRESS_POLICY, namespace, managed_labels()),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(selector(&[(LABEL_PROBE_ROLE, PROBE_ROLE_CONTROL)])),
            policy_types: Some(vec!["Egress".to_string()]),
            ingress: None,
            egress: Some(vec![
                dns_egress_rule(),
                NetworkPolicyEgressRule {
                    to: Some(vec![NetworkPolicyPeer {
                        pod_selector: Some(selector(&[(LABEL_PROBE_ROLE, PROBE_ROLE_CANARY)])),
                        ..Default::default()
                    }]),
                    ports: Some(vec![tcp(PROBE_CANARY_PORT)]),
                },
            ]),
        }),
    }
}

/// The canary accepts connections from any Pod of the namespace, so the only
/// thing between the plugin-labelled probe and the canary is the plugin's
/// EGRESS policy — the thing under test.
pub fn probe_canary_ingress_policy(namespace: &str) -> NetworkPolicy {
    NetworkPolicy {
        metadata: meta(PROBE_CANARY_INGRESS_POLICY, namespace, managed_labels()),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(selector(&[(LABEL_PROBE_ROLE, PROBE_ROLE_CANARY)])),
            policy_types: Some(vec!["Ingress".to_string()]),
            ingress: Some(vec![NetworkPolicyIngressRule {
                from: Some(vec![NetworkPolicyPeer {
                    pod_selector: Some(LabelSelector::default()),
                    ..Default::default()
                }]),
                ports: Some(vec![tcp(PROBE_CANARY_PORT)]),
            }]),
            egress: None,
        }),
    }
}

/// Every NetworkPolicy of the namespace baseline, in apply order: the deny
/// first, so there is never a moment with an allowance and no deny.
pub fn baseline_policies(
    namespace: &str,
    broker: &BrokerPeer,
    broker_port: u16,
) -> Vec<NetworkPolicy> {
    vec![
        default_deny_policy(namespace),
        plugin_egress_policy(namespace, broker, broker_port),
        probe_control_egress_policy(namespace),
        probe_canary_ingress_policy(namespace),
    ]
}

/// The Pod-level `restricted` profile: non-root, a fixed unprivileged
/// uid/gid, RuntimeDefault seccomp.
///
/// `fsGroup` is the Plugin's gid, so a storage claim whose volume type
/// supports ownership management is group-writable by the Plugin; with
/// `OnRootMismatch` the kubelet re-owns it only when its root is not
/// already the Plugin's, not on every start of a large volume.
pub fn restricted_pod_security_context() -> PodSecurityContext {
    PodSecurityContext {
        run_as_non_root: Some(true),
        run_as_user: Some(PLUGIN_UID),
        run_as_group: Some(PLUGIN_UID),
        fs_group: Some(PLUGIN_UID),
        fs_group_change_policy: Some("OnRootMismatch".to_string()),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The container-level `restricted` profile, plus a read-only root
/// filesystem. No grants, ever: unlike a Step (ADR-0039) a Plugin has no
/// admission path that could bless one.
pub fn restricted_container_security_context() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        privileged: Some(false),
        read_only_root_filesystem: Some(true),
        run_as_non_root: Some(true),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: None,
        }),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `plugin-<id>`: no token automount and no RBAC (nothing binds it) — the
/// only credential its Pod gets is the projected broker token.
pub fn service_account(namespace: &str, installation: &PluginInstallation) -> ServiceAccount {
    ServiceAccount {
        metadata: meta(
            &workload_name(&installation.id),
            namespace,
            installation_labels(&installation.id),
        ),
        automount_service_account_token: Some(false),
        ..Default::default()
    }
}

/// The Plugin's environment: its approved config, then core's own variables
/// LAST. Config keys under `SCARAB_` are refused when the manifest is
/// validated; they are dropped here as well, so no config value can ever
/// re-aim the broker URL or the token file.
fn plugin_env(installation: &PluginInstallation, broker_url: &str) -> Vec<EnvVar> {
    let env = |name: &str, value: &str| EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..Default::default()
    };
    let mut vars: Vec<EnvVar> = installation
        .config
        .iter()
        .filter(|(k, _)| !k.starts_with(RESERVED_CONFIG_KEY_PREFIX))
        .map(|(k, v)| env(k, v))
        .collect();
    vars.push(env(PLUGIN_ENV_BROKER_URL, broker_url));
    vars.push(env(
        PLUGIN_ENV_BROKER_TOKEN_FILE,
        &format!("{BROKER_TOKEN_DIR}/{BROKER_TOKEN_FILE}"),
    ));
    vars
}

/// `plugin-<id>`: one replica, `Recreate` — one instance per installation,
/// never two overlapping during a rollout, because a reporter's sequences
/// are per installation (ADR-0070 §11). Runs as `plugin-<id>` under the
/// restricted profile with a read-only root, no service links (they would
/// list every Service's address), and only the broker token mounted.
///
/// Before the Plugin starts, the isolation self-check initContainer
/// ([`crate::probe::self_check_container`], `probe_image`) proves THIS Pod's
/// network on THIS node: broker reachable, nothing else. `broker_url` is the
/// broker as a Pod dials it — by IP, since a Plugin has no DNS.
pub fn deployment(
    namespace: &str,
    installation: &PluginInstallation,
    broker_url: &str,
    probe_image: &str,
) -> Deployment {
    let name = workload_name(&installation.id);
    let mut pod_labels = installation_labels(&installation.id);
    pod_labels.insert(LABEL_PLUGIN_WORKLOAD.to_string(), "true".to_string());
    let mut annotations = BTreeMap::from([
        ("scarab.io/org".to_string(), installation.org.clone()),
        (
            "scarab.io/installation-name".to_string(),
            installation.name.clone(),
        ),
        ("scarab.io/plugin".to_string(), installation.plugin.clone()),
    ]);
    // On the Pod template too, so an upgrade to another approved manifest
    // rolls the Pod even if the image were the same.
    let template_annotations = BTreeMap::from([(
        "scarab.io/manifest-sha256".to_string(),
        installation.manifest_sha256.clone(),
    )]);
    annotations.extend(template_annotations.clone());
    let quantity = |q: &str| Quantity(q.to_string());
    let mut volume_mounts = vec![
        VolumeMount {
            name: "broker-token".to_string(),
            mount_path: BROKER_TOKEN_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        // The root is read-only; a Plugin gets a small scratch /tmp and,
        // if its installation names one, its storage claim.
        VolumeMount {
            name: "tmp".to_string(),
            mount_path: PLUGIN_TMP_DIR.to_string(),
            ..Default::default()
        },
    ];
    let mut volumes = vec![
        Volume {
            name: "broker-token".to_string(),
            projected: Some(ProjectedVolumeSource {
                sources: Some(vec![VolumeProjection {
                    service_account_token: Some(ServiceAccountTokenProjection {
                        audience: Some(BROKER_TOKEN_AUDIENCE.to_string()),
                        expiration_seconds: Some(BROKER_TOKEN_EXPIRATION_SECONDS),
                        path: BROKER_TOKEN_FILE.to_string(),
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: "tmp".to_string(),
            empty_dir: Some(EmptyDirVolumeSource {
                size_limit: Some(quantity("64Mi")),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];
    // ADR-0074 §7: the operator's claim, read-write. The runtime has
    // checked it exists and is annotated for this installation
    // ([`storage_claim_refusal`]) before it applies this.
    if let Some(storage) = &installation.storage {
        volume_mounts.push(VolumeMount {
            name: STORAGE_VOLUME.to_string(),
            mount_path: storage.mount_path.clone(),
            read_only: Some(false),
            ..Default::default()
        });
        volumes.push(Volume {
            name: STORAGE_VOLUME.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: storage.claim.clone(),
                read_only: Some(false),
            }),
            ..Default::default()
        });
    }
    Deployment {
        metadata: ObjectMeta {
            annotations: Some(annotations),
            ..meta(&name, namespace, pod_labels.clone())
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            revision_history_limit: Some(2),
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    LABEL_INSTALLATION.to_string(),
                    installation.id.clone(),
                )])),
                ..Default::default()
            },
            strategy: Some(DeploymentStrategy {
                type_: Some("Recreate".to_string()),
                rolling_update: None,
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(pod_labels),
                    annotations: Some(template_annotations),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    service_account_name: Some(name.clone()),
                    automount_service_account_token: Some(false),
                    enable_service_links: Some(false),
                    security_context: Some(restricted_pod_security_context()),
                    init_containers: Some(vec![crate::probe::self_check_container(
                        probe_image,
                        broker_url,
                    )]),
                    containers: vec![Container {
                        name: "plugin".to_string(),
                        image: Some(installation.image.clone()),
                        image_pull_policy: Some("IfNotPresent".to_string()),
                        env: Some(plugin_env(installation, broker_url)),
                        security_context: Some(restricted_container_security_context()),
                        resources: Some(ResourceRequirements {
                            requests: Some(BTreeMap::from([
                                ("cpu".to_string(), quantity("50m")),
                                ("memory".to_string(), quantity("64Mi")),
                            ])),
                            // CPU too: a spinning Plugin gets half a core,
                            // never the node.
                            limits: Some(BTreeMap::from([
                                ("cpu".to_string(), quantity(PLUGIN_CPU_LIMIT)),
                                ("memory".to_string(), quantity("256Mi")),
                            ])),
                            ..Default::default()
                        }),
                        volume_mounts: Some(volume_mounts),
                        ..Default::default()
                    }],
                    volumes: Some(volumes),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Why the runtime must not mount `claim` (as read from the plugin
/// namespace; `None` = it does not exist) for `installation`'s `storage`,
/// or `None` when it may. The namespace is shared by every Org, so a claim
/// is mounted only once an operator has annotated it for this installation
/// ([`PLUGIN_STORAGE_OWNER_ANNOTATION`] = `<org>/<name>`).
pub fn storage_claim_refusal(
    namespace: &str,
    installation: &PluginInstallation,
    storage: &PluginStorage,
    claim: Option<&PersistentVolumeClaim>,
) -> Option<String> {
    let owner = installation.storage_owner();
    let annotate = format!("annotated {PLUGIN_STORAGE_OWNER_ANNOTATION}={owner}");
    let Some(claim) = claim else {
        return Some(format!(
            "storage: PersistentVolumeClaim `{}` does not exist in namespace `{namespace}`. \
             Scarab does not create it: an operator creates it there, {annotate}",
            storage.claim
        ));
    };
    if claim.metadata.deletion_timestamp.is_some() {
        return Some(format!(
            "storage: PersistentVolumeClaim `{}` in namespace `{namespace}` is being deleted",
            storage.claim
        ));
    }
    let said = claim
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(PLUGIN_STORAGE_OWNER_ANNOTATION));
    if said != Some(&owner) {
        return Some(format!(
            "storage: PersistentVolumeClaim `{}` in namespace `{namespace}` is not {annotate} \
             ({}), so the plugin does not start with it. The plugin namespace is shared by \
             every org; the annotation is how an operator says the claim is for this \
             installation",
            storage.claim,
            said.map_or("it has no such annotation".to_string(), |v| format!(
                "it says `{}`",
                v.escape_debug()
            ))
        ));
    }
    None
}

/// What a Plugin Pod that is not up yet is waiting on, when it is its
/// storage claim: one that has not bound to a volume. (A claim whose
/// StorageClass binds on first use stays `Pending` until the Pod is
/// scheduled, so this is reported as waiting, not failing.)
pub fn storage_claim_waiting(
    storage: &PluginStorage,
    claim: &PersistentVolumeClaim,
) -> Option<String> {
    let phase = claim
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("Pending");
    (phase != "Bound").then(|| {
        format!(
            "waiting for PersistentVolumeClaim `{}` to bind (phase {phase}); Scarab does not \
             provision it",
            storage.claim
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_plugin::{InstallationState, PluginCapability, PluginManifest, RuntimeStatus};

    const BROKER_URL: &str = "http://10.96.0.20:8090";
    const PROBE_IMAGE: &str = "busybox@sha256:0000";

    fn installation() -> PluginInstallation {
        PluginInstallation {
            id: "0123456789".into(),
            org: "acme".into(),
            name: "argocd-prod".into(),
            plugin: "argocd".into(),
            image: format!("ghcr.io/acme/argocd@sha256:{}", "a".repeat(64)),
            manifest: PluginManifest {
                plugin: "argocd".into(),
                capabilities: vec![PluginCapability::UseConnection],
                connections: vec![],
                config_keys: vec!["ARGOCD_UI_URL".into()],
            },
            manifest_sha256: "f".repeat(64),
            config: BTreeMap::from([(
                "ARGOCD_UI_URL".to_string(),
                "https://argo.example".to_string(),
            )]),
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

    fn pod_spec(d: &Deployment) -> &PodSpec {
        d.spec.as_ref().unwrap().template.spec.as_ref().unwrap()
    }

    fn env_of(d: &Deployment) -> BTreeMap<String, String> {
        pod_spec(d).containers[0]
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.clone(), e.value.clone().unwrap()))
            .collect()
    }

    #[test]
    fn the_service_account_has_no_token_automount() {
        let sa = service_account("scarab-plugins", &installation());
        assert_eq!(sa.metadata.name.as_deref(), Some("plugin-0123456789"));
        assert_eq!(sa.automount_service_account_token, Some(false));
        assert_eq!(
            sa.metadata.labels.unwrap().get(LABEL_INSTALLATION),
            Some(&"0123456789".to_string())
        );
    }

    #[test]
    fn the_deployment_is_one_recreated_replica_as_its_own_service_account() {
        let d = deployment("scarab-plugins", &installation(), BROKER_URL, PROBE_IMAGE);
        let spec = d.spec.as_ref().unwrap();
        assert_eq!(d.metadata.name.as_deref(), Some("plugin-0123456789"));
        assert_eq!(spec.replicas, Some(1));
        assert_eq!(
            spec.strategy.as_ref().unwrap().type_.as_deref(),
            Some("Recreate")
        );
        let pod = pod_spec(&d);
        assert_eq!(
            pod.service_account_name.as_deref(),
            Some("plugin-0123456789")
        );
        assert_eq!(pod.automount_service_account_token, Some(false));
        assert_eq!(pod.enable_service_links, Some(false));
        assert_eq!(pod.host_network, None);
        let labels = spec
            .template
            .metadata
            .as_ref()
            .unwrap()
            .labels
            .as_ref()
            .unwrap();
        assert_eq!(labels.get(LABEL_PLUGIN_WORKLOAD), Some(&"true".to_string()));
        // The selector is a subset of the Pod's labels.
        for (k, v) in spec.selector.match_labels.as_ref().unwrap() {
            assert_eq!(labels.get(k), Some(v));
        }
    }

    #[test]
    fn the_plugin_runs_under_the_restricted_profile_with_a_read_only_root() {
        let d = deployment("scarab-plugins", &installation(), BROKER_URL, PROBE_IMAGE);
        let pod = pod_spec(&d);
        let psc = pod.security_context.as_ref().unwrap();
        assert_eq!(psc.run_as_non_root, Some(true));
        assert_eq!(psc.run_as_user, Some(PLUGIN_UID));
        assert_eq!(
            psc.seccomp_profile.as_ref().unwrap().type_,
            "RuntimeDefault"
        );
        let csc = pod.containers[0].security_context.as_ref().unwrap();
        assert_eq!(csc.allow_privilege_escalation, Some(false));
        assert_eq!(csc.privileged, Some(false));
        assert_eq!(csc.read_only_root_filesystem, Some(true));
        assert_eq!(csc.run_as_non_root, Some(true));
        let caps = csc.capabilities.as_ref().unwrap();
        assert_eq!(caps.drop.as_deref(), Some(&["ALL".to_string()][..]));
        assert_eq!(caps.add, None);
    }

    /// Every container of a Plugin Pod is bounded in CPU and memory — the
    /// Plugin and its self-check alike.
    #[test]
    fn every_container_of_a_plugin_pod_has_a_cpu_and_memory_limit() {
        let d = deployment("scarab-plugins", &installation(), BROKER_URL, PROBE_IMAGE);
        let pod = pod_spec(&d);
        let containers = pod
            .containers
            .iter()
            .chain(pod.init_containers.iter().flatten());
        let mut seen = 0;
        for c in containers {
            let limits = c.resources.as_ref().unwrap().limits.as_ref().unwrap();
            assert_eq!(
                limits["cpu"],
                Quantity(PLUGIN_CPU_LIMIT.to_string()),
                "{}",
                c.name
            );
            assert!(limits.contains_key("memory"), "{}", c.name);
            seen += 1;
        }
        assert_eq!(seen, 2);
    }

    #[test]
    fn the_only_credential_is_a_short_lived_broker_audience_token() {
        let d = deployment("scarab-plugins", &installation(), BROKER_URL, PROBE_IMAGE);
        let pod = pod_spec(&d);
        let volumes = pod.volumes.as_ref().unwrap();
        let projections: Vec<&ServiceAccountTokenProjection> = volumes
            .iter()
            .filter_map(|v| v.projected.as_ref())
            .flat_map(|p| p.sources.as_ref().unwrap())
            .filter_map(|s| s.service_account_token.as_ref())
            .collect();
        assert_eq!(projections.len(), 1);
        assert_eq!(projections[0].audience.as_deref(), Some("scarab-broker"));
        assert_eq!(projections[0].expiration_seconds, Some(600));
        // No Secret volume, no other projected source: nothing else to hold.
        assert!(volumes.iter().all(|v| v.secret.is_none()));
        let mounts = pod.containers[0].volume_mounts.as_ref().unwrap();
        let token_mount = mounts.iter().find(|m| m.name == "broker-token").unwrap();
        assert_eq!(token_mount.read_only, Some(true));
    }

    #[test]
    fn the_env_is_the_approved_config_then_cores_own_variables() {
        let mut inst = installation();
        // A key validation would have refused can still never re-aim the
        // broker.
        inst.config
            .insert("SCARAB_BROKER_URL".into(), "http://evil:1".into());
        let d = deployment("scarab-plugins", &inst, BROKER_URL, PROBE_IMAGE);
        let env = env_of(&d);
        assert_eq!(env["ARGOCD_UI_URL"], "https://argo.example");
        assert_eq!(env["SCARAB_BROKER_URL"], BROKER_URL);
        assert_eq!(
            env["SCARAB_BROKER_TOKEN_FILE"],
            "/var/run/secrets/scarab/broker/token"
        );
        let names: Vec<_> = pod_spec(&d).containers[0]
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(
            names.iter().filter(|n| **n == "SCARAB_BROKER_URL").count(),
            1
        );
    }

    fn with_storage(mount_path: Option<&str>) -> PluginInstallation {
        PluginInstallation {
            storage: Some(PluginStorage::new("argocd-state", mount_path).unwrap()),
            ..installation()
        }
    }

    /// Without storage a Plugin has the broker token and /tmp, nothing else;
    /// with it, the claim is one more read-write volume — never a hostPath,
    /// and the restricted profile is unchanged.
    #[test]
    fn storage_adds_one_read_write_claim_volume_and_nothing_else() {
        let plain = deployment("scarab-plugins", &installation(), BROKER_URL, PROBE_IMAGE);
        let pod = pod_spec(&plain);
        let mounts: Vec<_> = pod.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mount_path.as_str())
            .collect();
        assert_eq!(mounts, [BROKER_TOKEN_DIR, PLUGIN_TMP_DIR]);
        assert!(pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .all(|v| v.persistent_volume_claim.is_none()));

        let stored = deployment(
            "scarab-plugins",
            &with_storage(Some("/var/lib/argocd")),
            BROKER_URL,
            PROBE_IMAGE,
        );
        let pod = pod_spec(&stored);
        let volumes = pod.volumes.as_ref().unwrap();
        assert_eq!(volumes.len(), 3);
        let claim = volumes.iter().find(|v| v.name == STORAGE_VOLUME).unwrap();
        let pvc = claim.persistent_volume_claim.as_ref().unwrap();
        assert_eq!(pvc.claim_name, "argocd-state");
        assert_eq!(pvc.read_only, Some(false));
        assert!(volumes.iter().all(|v| v.host_path.is_none()));
        let mount = pod.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == STORAGE_VOLUME)
            .unwrap();
        assert_eq!(mount.mount_path, "/var/lib/argocd");
        assert_eq!(mount.read_only, Some(false));
        // The self-check initContainer never sees the claim.
        assert!(pod.init_containers.as_ref().unwrap()[0]
            .volume_mounts
            .is_none());
        // Same Pod-level profile with and without; the claim is writable by
        // the Plugin's gid.
        assert_eq!(pod.security_context, pod_spec(&plain).security_context);
        let psc = pod.security_context.as_ref().unwrap();
        assert_eq!(psc.fs_group, Some(PLUGIN_UID));
        assert_eq!(
            psc.fs_group_change_policy.as_deref(),
            Some("OnRootMismatch")
        );
        assert_eq!(
            pod.containers[0].security_context,
            Some(restricted_container_security_context())
        );
    }

    #[test]
    fn storage_mounts_at_data_by_default() {
        let d = deployment(
            "scarab-plugins",
            &with_storage(None),
            BROKER_URL,
            PROBE_IMAGE,
        );
        let mount = pod_spec(&d).containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == STORAGE_VOLUME)
            .unwrap()
            .mount_path
            .clone();
        assert_eq!(mount, "/data");
    }

    /// Every path this Deployment mounts itself is one a storage mount path
    /// may not overlap: the reserved list in `scarab-plugin` covers it.
    #[test]
    fn every_runtime_mount_is_reserved_from_storage() {
        let d = deployment("scarab-plugins", &installation(), BROKER_URL, PROBE_IMAGE);
        for m in pod_spec(&d).containers[0].volume_mounts.as_ref().unwrap() {
            assert!(
                scarab_plugin::validate_storage_mount_path(&m.mount_path).is_err(),
                "{} must be refused as a storage mount path",
                m.mount_path
            );
        }
    }

    fn claim(annotation: Option<&str>, phase: Option<&str>) -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("argocd-state".into()),
                annotations: annotation.map(|v| {
                    BTreeMap::from([(PLUGIN_STORAGE_OWNER_ANNOTATION.to_string(), v.to_string())])
                }),
                ..Default::default()
            },
            status: phase.map(
                |p| k8s_openapi::api::core::v1::PersistentVolumeClaimStatus {
                    phase: Some(p.to_string()),
                    ..Default::default()
                },
            ),
            ..Default::default()
        }
    }

    /// The plugin namespace is shared by every Org: a claim is mounted only
    /// once an operator has annotated it for THIS installation.
    #[test]
    fn a_claim_is_mounted_only_when_annotated_for_this_installation() {
        let inst = with_storage(None);
        let storage = inst.storage.clone().unwrap();
        let refusal = |c: Option<&PersistentVolumeClaim>| {
            storage_claim_refusal("scarab-plugins", &inst, &storage, c)
        };
        assert_eq!(refusal(Some(&claim(Some("acme/argocd-prod"), None))), None);

        let missing = refusal(None).unwrap();
        assert!(
            missing.contains("`argocd-state` does not exist"),
            "{missing}"
        );
        assert!(missing.contains("Scarab does not create it"), "{missing}");
        assert!(
            missing.contains("scarab.io/plugin-installation=acme/argocd-prod"),
            "{missing}"
        );

        for other in [
            None,
            Some("globex/argocd-prod"),
            Some("acme/argocd-staging"),
        ] {
            let why = refusal(Some(&claim(other, Some("Bound")))).unwrap();
            assert!(why.contains("`argocd-state`"), "{why}");
            assert!(why.contains("acme/argocd-prod"), "{why}");
        }
    }

    #[test]
    fn an_unbound_claim_is_waited_on_by_name() {
        let storage = PluginStorage::new("argocd-state", None).unwrap();
        assert_eq!(
            storage_claim_waiting(&storage, &claim(None, Some("Bound"))),
            None
        );
        for phase in [None, Some("Pending"), Some("Lost")] {
            let why = storage_claim_waiting(&storage, &claim(None, phase)).unwrap();
            assert!(why.contains("`argocd-state`"), "{why}");
        }
    }

    #[test]
    fn default_deny_selects_every_pod_both_ways_with_no_rule() {
        let np = default_deny_policy("scarab-plugins");
        let spec = np.spec.unwrap();
        assert_eq!(spec.pod_selector, Some(LabelSelector::default()));
        assert_eq!(
            spec.policy_types,
            Some(vec!["Ingress".to_string(), "Egress".to_string()])
        );
        assert_eq!(spec.ingress, None);
        assert_eq!(spec.egress, None);
    }

    /// The plugin's own Pod proves its isolation before the Plugin runs, as
    /// the probe image, with the broker URL and nothing of the Plugin's.
    #[test]
    fn the_plugin_starts_only_after_its_own_isolation_self_check() {
        let d = deployment("scarab-plugins", &installation(), BROKER_URL, PROBE_IMAGE);
        let pod = pod_spec(&d);
        let init = pod.init_containers.as_ref().unwrap();
        assert_eq!(init.len(), 1);
        assert_eq!(init[0].name, crate::probe::SELF_CHECK_CONTAINER);
        assert_eq!(init[0].image.as_deref(), Some(PROBE_IMAGE));
        let env: Vec<_> = init[0]
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(env, ["BROKER_URL"], "no Plugin config, no token");
        assert!(init[0].volume_mounts.is_none());
    }

    #[test]
    fn plugin_egress_allows_the_broker_port_only_not_even_dns() {
        let peer = BrokerPeer::Pods {
            namespace: "scarab".into(),
            labels: BTreeMap::from([("app.kubernetes.io/name".into(), "scarab".into())]),
        };
        let spec = plugin_egress_policy("scarab-plugins", &peer, 8090)
            .spec
            .unwrap();
        assert_eq!(
            spec.pod_selector.unwrap().match_labels.unwrap(),
            BTreeMap::from([(LABEL_PLUGIN_WORKLOAD.to_string(), "true".to_string())])
        );
        // Egress only: ingress stays at default-deny's nothing.
        assert_eq!(spec.policy_types, Some(vec!["Egress".to_string()]));
        assert_eq!(spec.ingress, None);
        let egress = spec.egress.unwrap();
        // One rule: the broker. No DNS — a recursive resolver is a tunnel out.
        assert_eq!(egress.len(), 1);
        assert!(egress
            .iter()
            .flat_map(|r| r.ports.iter().flatten())
            .all(|p| p.port != Some(IntOrString::Int(53))));
        let broker = &egress[0];
        let broker_to = &broker.to.as_ref().unwrap()[0];
        assert_eq!(
            broker_to
                .namespace_selector
                .as_ref()
                .unwrap()
                .match_labels
                .as_ref()
                .unwrap()["kubernetes.io/metadata.name"],
            "scarab"
        );
        assert_eq!(
            broker.ports.as_ref().unwrap()[0].port,
            Some(IntOrString::Int(8090))
        );
        // Every rule names a destination: none is an allow-all.
        assert!(egress
            .iter()
            .all(|r| r.to.as_ref().is_some_and(|t| !t.is_empty())));
    }

    #[test]
    fn a_cidr_broker_is_an_ip_block() {
        let spec = plugin_egress_policy(
            "scarab-plugins",
            &BrokerPeer::Cidr("192.168.5.2/32".into()),
            8090,
        )
        .spec
        .unwrap();
        let broker_to = &spec.egress.unwrap()[0].to.clone().unwrap()[0];
        assert_eq!(
            broker_to.ip_block.as_ref().map(|b| b.cidr.as_str()),
            Some("192.168.5.2/32")
        );
        assert!(broker_to.pod_selector.is_none() && broker_to.namespace_selector.is_none());
    }

    #[test]
    fn the_probe_allowances_never_select_a_plugin_workload() {
        for np in [
            probe_control_egress_policy("scarab-plugins"),
            probe_canary_ingress_policy("scarab-plugins"),
        ] {
            let selected = np.spec.unwrap().pod_selector.unwrap().match_labels.unwrap();
            assert!(selected.contains_key(LABEL_PROBE_ROLE));
            assert!(!selected.contains_key(LABEL_PLUGIN_WORKLOAD));
        }
    }

    #[test]
    fn the_baseline_applies_the_deny_first() {
        let names: Vec<_> = baseline_policies(
            "scarab-plugins",
            &BrokerPeer::Cidr("10.0.0.1/32".into()),
            8090,
        )
        .into_iter()
        .map(|np| np.metadata.name.unwrap())
        .collect();
        assert_eq!(
            names,
            [
                DEFAULT_DENY_POLICY,
                PLUGIN_EGRESS_POLICY,
                PROBE_CONTROL_EGRESS_POLICY,
                PROBE_CANARY_INGRESS_POLICY
            ]
        );
    }
}
