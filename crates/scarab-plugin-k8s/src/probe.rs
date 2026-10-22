//! The fail-closed isolation probe (ADR-0072 §9): before any Plugin starts,
//! prove the cluster enforces the namespace's NetworkPolicy.
//!
//! Three Pods, all in the plugin namespace, all under the `restricted`
//! profile:
//!
//! 1. a **canary** listening on TCP 8080 — the stand-in for "another Pod",
//!    e.g. another org's Plugin;
//! 2. a **control** Pod whose egress to the canary is allowed. It MUST
//!    connect. Without it, a plugin-side connection that fails proves
//!    nothing: the canary might simply be down, or the network broken;
//! 3. a **plugin-labelled** Pod — selected by `plugin-egress` exactly as a
//!    Plugin is. It MUST reach the broker's `healthz` and resolve DNS (the
//!    one allowance works), and MUST FAIL to reach the canary, the API
//!    server (`$KUBERNETES_SERVICE_HOST`, always injected by the kubelet) and
//!    `1.1.1.1:443` (the internet).
//!
//! Any deviation fails the probe and no Plugin starts — including a probe
//! that cannot finish. A cluster whose CNI ignores NetworkPolicy lets the
//! plugin-labelled Pod reach the canary, and fails here, loudly, rather than
//! running Plugins unprotected.
//!
//! Each Pod prints `SCARAB_PROBE <check> <reached|unreached>` lines; the
//! runtime reads their logs and [`assess`]es them. Pure: tested without a
//! cluster.

use std::collections::BTreeMap;
use std::net::IpAddr;

use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, Pod, PodSpec, Probe, TCPSocketAction, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::objects::{
    restricted_container_security_context, restricted_pod_security_context, LABEL_PROBE,
    LABEL_PROBE_ROLE, PROBE_CANARY_PORT, PROBE_ROLE_CANARY, PROBE_ROLE_CONTROL, PROBE_ROLE_PLUGIN,
};
use crate::{LABEL_MANAGED_BY, LABEL_PLUGIN_WORKLOAD, MANAGED_BY};

/// The default probe image (`SCARAB_PLUGIN_PROBE_IMAGE` overrides it): its
/// `httpd`, `nc -z`, and `wget` are all the probe uses. The same image the
/// proc-mode stack already pulls for its own discovery Pod.
pub const DEFAULT_PROBE_IMAGE: &str = "busybox:1.36";

/// The address the internet check dials.
pub const PROBE_INTERNET_ADDR: (&str, u16) = ("1.1.1.1", 443);

/// The name the DNS check resolves when the broker is addressed by IP (proc
/// mode): the API server's Service, which every cluster has. Resolved
/// through the libc resolver, so the Pod's search domains apply.
pub const PROBE_DNS_FALLBACK_NAME: &str = "kubernetes.default.svc";

/// A probe Pod must be done well within this — it is also the Pod's
/// `activeDeadlineSeconds`, so a probe abandoned by a crashed manager cleans
/// itself up.
pub const PROBE_POD_DEADLINE_SECONDS: i64 = 300;

const LINE_PREFIX: &str = "SCARAB_PROBE";

/// What the probe checks. The `wire` name is what the Pod prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProbeCheck {
    /// Control Pod → canary. MUST be reached.
    ControlReachesCanary,
    /// Plugin-labelled Pod → broker healthz. MUST be reached.
    Broker,
    /// Plugin-labelled Pod → cluster DNS. MUST be reached.
    Dns,
    /// Plugin-labelled Pod → canary. MUST NOT be reached.
    Canary,
    /// Plugin-labelled Pod → the API server. MUST NOT be reached.
    ApiServer,
    /// Plugin-labelled Pod → 1.1.1.1:443. MUST NOT be reached.
    Internet,
}

impl ProbeCheck {
    fn wire(self) -> &'static str {
        match self {
            ProbeCheck::ControlReachesCanary | ProbeCheck::Canary => "canary",
            ProbeCheck::Broker => "broker",
            ProbeCheck::Dns => "dns",
            ProbeCheck::ApiServer => "apiserver",
            ProbeCheck::Internet => "internet",
        }
    }

    fn must_reach(self) -> bool {
        matches!(
            self,
            ProbeCheck::ControlReachesCanary | ProbeCheck::Broker | ProbeCheck::Dns
        )
    }

    /// What a deviation means, for the operator reading `runtime_message`.
    fn deviation(self) -> &'static str {
        match self {
            ProbeCheck::ControlReachesCanary => {
                "inconclusive: the control Pod, whose egress to the canary is allowed, could not \
                 reach it — a blocked plugin connection would prove nothing"
            }
            ProbeCheck::Broker => {
                "a plugin cannot reach the broker's healthz — the plugin-egress allowance does not \
                 work (check SCARAB_PLUGIN_BROKER_URL / SCARAB_PLUGIN_BROKER_PEER)"
            }
            ProbeCheck::Dns => "a plugin cannot resolve names through the cluster DNS",
            ProbeCheck::Canary => {
                "NetworkPolicy is NOT enforced: a plugin-labelled Pod reached another Pod (the \
                 canary)"
            }
            ProbeCheck::ApiServer => "a plugin-labelled Pod reached the Kubernetes API server",
            ProbeCheck::Internet => "a plugin-labelled Pod reached 1.1.1.1:443 (the internet)",
        }
    }
}

const PLUGIN_SIDE_CHECKS: [ProbeCheck; 5] = [
    ProbeCheck::Broker,
    ProbeCheck::Dns,
    ProbeCheck::Canary,
    ProbeCheck::ApiServer,
    ProbeCheck::Internet,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    Passed,
    Failed,
}

/// The probe's outcome: passed only with no finding at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub verdict: ProbeVerdict,
    pub findings: Vec<String>,
}

impl ProbeReport {
    pub fn passed(&self) -> bool {
        self.verdict == ProbeVerdict::Passed
    }

    /// A probe that could not run to the end — a Pod that never started, a
    /// timeout, an API error. Fail closed: it is a failed probe.
    pub fn could_not_run(why: impl Into<String>) -> Self {
        Self {
            verdict: ProbeVerdict::Failed,
            findings: vec![format!("the probe could not run: {}", why.into())],
        }
    }

    /// One line for `runtime_message`.
    pub fn message(&self) -> String {
        match self.verdict {
            ProbeVerdict::Passed => "isolation probe passed".to_string(),
            ProbeVerdict::Failed => {
                format!("isolation probe failed: {}", self.findings.join("; "))
            }
        }
    }
}

/// `check -> reached?` from a probe Pod's log. A repeated line keeps the
/// last; anything else is ignored.
fn parse_log(log: &str) -> BTreeMap<String, bool> {
    let mut seen = BTreeMap::new();
    for line in log.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some(LINE_PREFIX) {
            continue;
        }
        let (Some(check), Some(result)) = (words.next(), words.next()) else {
            continue;
        };
        match result {
            "reached" => seen.insert(check.to_string(), true),
            "unreached" => seen.insert(check.to_string(), false),
            _ => None,
        };
    }
    seen
}

/// Judge the two probe logs. Every check must report and come out as
/// expected; a missing report is a failure (the Pod did not finish its
/// script), never a pass.
pub fn assess(control_log: &str, plugin_log: &str) -> ProbeReport {
    let control = parse_log(control_log);
    let plugin = parse_log(plugin_log);
    let mut findings = Vec::new();
    let mut judge = |check: ProbeCheck, seen: &BTreeMap<String, bool>, pod: &str| match seen
        .get(check.wire())
    {
        Some(&reached) if reached == check.must_reach() => {}
        Some(_) => findings.push(check.deviation().to_string()),
        None => findings.push(format!(
            "the {pod} Pod never reported its `{}` check",
            check.wire()
        )),
    };
    judge(ProbeCheck::ControlReachesCanary, &control, "control");
    for check in PLUGIN_SIDE_CHECKS {
        judge(check, &plugin, "plugin-labelled");
    }
    ProbeReport {
        verdict: if findings.is_empty() {
            ProbeVerdict::Passed
        } else {
            ProbeVerdict::Failed
        },
        findings,
    }
}

/// The name the DNS check resolves: the broker's host when it is a name
/// (then the broker check needs DNS anyway), else a name every cluster has.
pub fn dns_check_name(broker_url: &str) -> String {
    let rest = broker_url
        .split_once("://")
        .map_or(broker_url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    if host.is_empty() || host.parse::<IpAddr>().is_ok() {
        PROBE_DNS_FALLBACK_NAME.to_string()
    } else {
        host.to_string()
    }
}

/// `isolation-probe-<nonce>-<role>`.
pub fn probe_pod_name(nonce: &str, role: &str) -> String {
    format!("isolation-probe-{nonce}-{role}")
}

fn probe_labels(nonce: &str, role: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_PROBE.to_string(), nonce.to_string()),
        (LABEL_PROBE_ROLE.to_string(), role.to_string()),
        (LABEL_MANAGED_BY.to_string(), MANAGED_BY.to_string()),
    ])
}

fn env(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..Default::default()
    }
}

/// One probe Pod: restricted, read-only root with a scratch `/tmp`, no
/// ServiceAccount token, no service links, never restarted, bounded by a
/// deadline.
fn probe_pod(
    namespace: &str,
    nonce: &str,
    role: &str,
    image: &str,
    labels: BTreeMap<String, String>,
    container: Container,
) -> Pod {
    Pod {
        metadata: ObjectMeta {
            name: Some(probe_pod_name(nonce, role)),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PodSpec {
            automount_service_account_token: Some(false),
            enable_service_links: Some(false),
            restart_policy: Some("Never".to_string()),
            active_deadline_seconds: Some(PROBE_POD_DEADLINE_SECONDS),
            termination_grace_period_seconds: Some(0),
            security_context: Some(restricted_pod_security_context()),
            containers: vec![Container {
                name: "probe".to_string(),
                image: Some(image.to_string()),
                image_pull_policy: Some("IfNotPresent".to_string()),
                security_context: Some(restricted_container_security_context()),
                volume_mounts: Some(vec![VolumeMount {
                    name: "tmp".to_string(),
                    mount_path: "/tmp".to_string(),
                    ..Default::default()
                }]),
                ..container
            }],
            volumes: Some(vec![Volume {
                name: "tmp".to_string(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The canary: an HTTP listener on 8080, Ready once it accepts.
pub fn canary_pod(namespace: &str, nonce: &str, image: &str) -> Pod {
    probe_pod(
        namespace,
        nonce,
        PROBE_ROLE_CANARY,
        image,
        probe_labels(nonce, PROBE_ROLE_CANARY),
        Container {
            command: Some(vec![
                "httpd".to_string(),
                "-f".to_string(),
                "-p".to_string(),
                PROBE_CANARY_PORT.to_string(),
                "-h".to_string(),
                "/tmp".to_string(),
            ]),
            readiness_probe: Some(Probe {
                tcp_socket: Some(TCPSocketAction {
                    port: IntOrString::Int(i32::from(PROBE_CANARY_PORT)),
                    ..Default::default()
                }),
                period_seconds: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
}

/// `reach <check> <host> <port>`: a TCP connect, 3 s, nothing sent.
const REACH_FN: &str = r#"reach() { if nc -z -w 3 "$2" "$3" >/dev/null 2>&1; then echo "SCARAB_PROBE $1 reached"; else echo "SCARAB_PROBE $1 unreached"; fi; }"#;

/// The control Pod: retries for a while — the canary may have only just
/// become Ready, and a CNI may take a moment to program a new Pod — and
/// reports `canary`.
pub fn control_pod(namespace: &str, nonce: &str, image: &str, canary_ip: &str) -> Pod {
    let script = format!(
        r#"i=0; while [ $i -lt 15 ]; do if nc -z -w 3 "$CANARY_IP" {port} >/dev/null 2>&1; then echo "SCARAB_PROBE canary reached"; exit 0; fi; i=$((i+1)); sleep 2; done; echo "SCARAB_PROBE canary unreached""#,
        port = PROBE_CANARY_PORT
    );
    probe_pod(
        namespace,
        nonce,
        PROBE_ROLE_CONTROL,
        image,
        probe_labels(nonce, PROBE_ROLE_CONTROL),
        Container {
            command: Some(vec!["sh".to_string(), "-c".to_string(), script]),
            env: Some(vec![env("CANARY_IP", canary_ip)]),
            ..Default::default()
        },
    )
}

/// The plugin-labelled Pod: labelled `scarab.io/plugin-workload=true`, so
/// `plugin-egress` selects it exactly as it selects a Plugin, but carrying
/// no installation label (the reconcile loop never mistakes it for one).
///
/// The allowed checks run FIRST, with retries: they also absorb the time a
/// CNI takes to program a new Pod, so the forbidden checks after them run
/// against the policy as enforced, not against a Pod the CNI has not caught
/// up with. (If a CNI is still open then, the probe fails — the safe way.)
pub fn plugin_probe_pod(
    namespace: &str,
    nonce: &str,
    image: &str,
    canary_ip: &str,
    broker_url: &str,
) -> Pod {
    let mut labels = probe_labels(nonce, PROBE_ROLE_PLUGIN);
    labels.insert(LABEL_PLUGIN_WORKLOAD.to_string(), "true".to_string());
    let (internet_host, internet_port) = PROBE_INTERNET_ADDR;
    // DNS: busybox wget resolves through libc (search domains apply) and
    // says `bad address` when it cannot; anything else — even a refused
    // connection to port 1 — means the name resolved. An API server address
    // the kubelet did not inject reports `unknown`, which `assess` counts as
    // never reported: a check that could not run is a failure, not a pass.
    let script = format!(
        r#"{REACH_FN}
broker=unreached; i=0
while [ $i -lt 15 ]; do
  if [ "$(wget -q -T 5 -O - "$BROKER_URL/_scarab/plugin/v1/healthz" 2>/dev/null)" = ok ]; then broker=reached; break; fi
  i=$((i+1)); sleep 2
done
echo "SCARAB_PROBE broker $broker"
case "$(wget -q -T 3 -O /dev/null "http://$DNS_NAME:1/" 2>&1)" in
  *"bad address"*) echo "SCARAB_PROBE dns unreached" ;;
  *) echo "SCARAB_PROBE dns reached" ;;
esac
reach canary "$CANARY_IP" {canary_port}
if [ -z "$KUBERNETES_SERVICE_HOST" ]; then echo "SCARAB_PROBE apiserver unknown"; else reach apiserver "$KUBERNETES_SERVICE_HOST" "$KUBERNETES_SERVICE_PORT"; fi
reach internet {internet_host} {internet_port}
"#,
        canary_port = PROBE_CANARY_PORT,
    );
    probe_pod(
        namespace,
        nonce,
        PROBE_ROLE_PLUGIN,
        image,
        labels,
        Container {
            command: Some(vec!["sh".to_string(), "-c".to_string(), script]),
            env: Some(vec![
                env("CANARY_IP", canary_ip),
                env("BROKER_URL", broker_url.trim_end_matches('/')),
                env("DNS_NAME", &dns_check_name(broker_url)),
            ]),
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTROL_OK: &str = "SCARAB_PROBE canary reached\n";
    const PLUGIN_OK: &str = "SCARAB_PROBE broker reached\nSCARAB_PROBE dns reached\n\
        SCARAB_PROBE canary unreached\nSCARAB_PROBE apiserver unreached\n\
        SCARAB_PROBE internet unreached\n";

    #[test]
    fn an_enforcing_cluster_passes() {
        let report = assess(CONTROL_OK, PLUGIN_OK);
        assert!(report.passed(), "{report:?}");
        assert_eq!(report.message(), "isolation probe passed");
    }

    #[test]
    fn a_plugin_reaching_the_canary_means_networkpolicy_is_not_enforced() {
        let leaky = PLUGIN_OK.replace("canary unreached", "canary reached");
        let report = assess(CONTROL_OK, &leaky);
        assert_eq!(report.verdict, ProbeVerdict::Failed);
        assert!(report.message().contains("NOT enforced"), "{report:?}");
    }

    #[test]
    fn every_forbidden_destination_fails_the_probe() {
        for check in ["apiserver", "internet"] {
            let leaky =
                PLUGIN_OK.replace(&format!("{check} unreached"), &format!("{check} reached"));
            assert!(!assess(CONTROL_OK, &leaky).passed(), "{check}");
        }
    }

    #[test]
    fn a_broken_allowance_fails_the_probe() {
        for check in ["broker", "dns"] {
            let broken =
                PLUGIN_OK.replace(&format!("{check} reached"), &format!("{check} unreached"));
            assert!(!assess(CONTROL_OK, &broken).passed(), "{check}");
        }
    }

    #[test]
    fn an_unreachable_canary_makes_the_probe_inconclusive_not_passed() {
        let report = assess("SCARAB_PROBE canary unreached\n", PLUGIN_OK);
        assert!(!report.passed());
        assert!(report.message().contains("inconclusive"));
    }

    #[test]
    fn a_check_that_never_reported_is_a_failure() {
        let truncated: String = PLUGIN_OK.lines().take(3).collect::<Vec<_>>().join("\n");
        let report = assess(CONTROL_OK, &truncated);
        assert!(!report.passed());
        assert!(report.message().contains("never reported its `apiserver`"));
        assert!(!assess("", "").passed());
        assert!(!ProbeReport::could_not_run("timeout").passed());
    }

    #[test]
    fn noise_in_the_log_is_ignored() {
        let noisy = format!("wget: bad address 'x'\nSCARAB_PROBE bogus\n{PLUGIN_OK}");
        assert!(assess(CONTROL_OK, &noisy).passed());
    }

    #[test]
    fn the_dns_check_resolves_the_broker_name_or_a_name_every_cluster_has() {
        assert_eq!(
            dns_check_name("http://scarab-broker.scarab.svc:8090"),
            "scarab-broker.scarab.svc"
        );
        assert_eq!(
            dns_check_name("http://host.docker.internal:8090/"),
            "host.docker.internal"
        );
        assert_eq!(
            dns_check_name("http://192.168.5.2:8090"),
            PROBE_DNS_FALLBACK_NAME
        );
        assert_eq!(
            dns_check_name("http://[fd00::1]:8090"),
            PROBE_DNS_FALLBACK_NAME
        );
    }

    #[test]
    fn only_the_plugin_side_pod_is_selected_as_a_plugin() {
        let canary = canary_pod("scarab-plugins", "n1", DEFAULT_PROBE_IMAGE);
        let control = control_pod("scarab-plugins", "n1", DEFAULT_PROBE_IMAGE, "10.0.0.9");
        let plugin = plugin_probe_pod(
            "scarab-plugins",
            "n1",
            DEFAULT_PROBE_IMAGE,
            "10.0.0.9",
            "http://broker:8090",
        );
        let labels = |p: &Pod| p.metadata.labels.clone().unwrap();
        assert!(!labels(&canary).contains_key(LABEL_PLUGIN_WORKLOAD));
        assert!(!labels(&control).contains_key(LABEL_PLUGIN_WORKLOAD));
        assert_eq!(
            labels(&plugin).get(LABEL_PLUGIN_WORKLOAD),
            Some(&"true".to_string())
        );
        // Never mistaken for an installation's workload.
        assert!(!labels(&plugin).contains_key(crate::LABEL_INSTALLATION));
        for pod in [&canary, &control, &plugin] {
            let spec = pod.spec.as_ref().unwrap();
            assert_eq!(spec.automount_service_account_token, Some(false));
            assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
            assert_eq!(
                spec.active_deadline_seconds,
                Some(PROBE_POD_DEADLINE_SECONDS)
            );
            let csc = spec.containers[0].security_context.as_ref().unwrap();
            assert_eq!(csc.allow_privilege_escalation, Some(false));
            assert_eq!(csc.read_only_root_filesystem, Some(true));
        }
    }

    #[test]
    fn the_plugin_side_script_checks_all_five_destinations() {
        let plugin = plugin_probe_pod("ns", "n1", "img", "10.0.0.9", "http://broker:8090");
        let script = &plugin.spec.unwrap().containers[0].command.clone().unwrap()[2];
        for needle in [
            "SCARAB_PROBE broker",
            "SCARAB_PROBE dns",
            "reach canary",
            "reach apiserver \"$KUBERNETES_SERVICE_HOST\"",
            "reach internet 1.1.1.1 443",
        ] {
            assert!(script.contains(needle), "{needle} missing from {script}");
        }
    }
}
