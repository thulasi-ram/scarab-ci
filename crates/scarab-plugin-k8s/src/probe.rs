//! The fail-closed isolation probe (ADR-0072 §9): before any Plugin starts,
//! prove the cluster enforces the namespace's NetworkPolicy.
//!
//! Three Pods, all in the plugin namespace, all under the `restricted`
//! profile:
//!
//! 1. a **canary** listening on TCP 8080 — the stand-in for "another Pod",
//!    e.g. another org's Plugin;
//! 2. a **control** Pod whose egress to the canary AND to the cluster DNS is
//!    allowed. It MUST connect, and MUST get a DNS answer. Without it, a
//!    plugin-side failure proves nothing: the canary might simply be down,
//!    the resolver broken, or the image's `nslookup` unable to say;
//! 3. a **plugin-labelled** Pod — selected by `plugin-egress` exactly as a
//!    Plugin is. It MUST reach the broker's `healthz` (the one allowance
//!    works), and MUST FAIL to reach the canary, the API server
//!    (`$KUBERNETES_SERVICE_HOST`, always injected by the kubelet),
//!    `1.1.1.1:443` (the internet), the cloud metadata service
//!    (`169.254.169.254:80`), and MUST get no DNS answer at all — the
//!    cluster resolver forwards any name upstream, so DNS would be a tunnel
//!    out. A Plugin needs no DNS: it is handed the broker as an IP.
//!
//! Any deviation fails the probe and no Plugin starts — including a probe
//! that cannot finish. A cluster whose CNI ignores NetworkPolicy lets the
//! plugin-labelled Pod reach the canary, and fails here, loudly, rather than
//! running Plugins unprotected.
//!
//! A CNI programs a NEW Pod's policy asynchronously (kindnet, seen: the
//! plugin-labelled Pod reached DNS, the API server and the internet in its
//! first second, and was blocked a few seconds later). So the plugin side
//! does not judge one early pass: it repeats its checks until ONE pass has
//! the broker reached and every forbidden destination unreached, or
//! [`PROBE_ENFORCEMENT_DEADLINE_SECONDS`] pass — and then the last pass is
//! the verdict. What it proves is "this cluster enforces the policy, within
//! the deadline"; a CNI that never enforces still fails, at the deadline.
//!
//! The probe proves one node, once. So every Plugin Pod also runs the
//! plugin-side checks itself, as an initContainer ([`self_check_container`])
//! before the Plugin starts — on its own node, at its own start — so the
//! window before enforcement lands is the self-check's, never the Plugin's.
//! Both run the SAME loop ([`plugin_check_fns`]' `until_isolated`), so they
//! cannot drift; the namespace probe remains the negative control the
//! initContainer lacks (it has no canary).
//!
//! Each Pod prints `SCARAB_PROBE <check> <reached|unreached|unknown>` lines;
//! the runtime reads their logs and [`assess`]es them. Pure: tested without a
//! cluster.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, Pod, PodSpec, Probe, ResourceRequirements,
    TCPSocketAction, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::objects::{
    restricted_container_security_context, restricted_pod_security_context, LABEL_PROBE,
    LABEL_PROBE_ROLE, PROBE_CANARY_PORT, PROBE_ROLE_CANARY, PROBE_ROLE_CONTROL, PROBE_ROLE_PLUGIN,
};
use crate::{LABEL_MANAGED_BY, LABEL_PLUGIN_WORKLOAD, MANAGED_BY};

/// The default probe image (`SCARAB_PLUGIN_PROBE_IMAGE` overrides it): its
/// `httpd`, `nc -z`, `wget`, `nslookup` and `timeout` are all the probe and
/// the self-check use. Pinned by digest — the multi-arch index of
/// `busybox:1.36.1` — because the probe's verdict is only as good as the
/// image printing it: a tag a privileged step on the same node could retag
/// in containerd would let a forged image print "passed", and the
/// self-check runs this image in every Plugin Pod.
pub const DEFAULT_PROBE_IMAGE: &str =
    "busybox:1.36.1@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662";

/// The address the internet check dials.
pub const PROBE_INTERNET_ADDR: (&str, u16) = ("1.1.1.1", 443);

/// The cloud metadata service (AWS, GCP, Azure, OpenStack all answer here):
/// node credentials, if a Plugin can reach it.
pub const PROBE_METADATA_ADDR: (&str, u16) = ("169.254.169.254", 80);

/// The name the DNS check looks up. External, because the channel to close
/// is recursion out of the cluster; absolute (the trailing dot), so the
/// search domains are not walked. Any DNS answer counts — an address,
/// NXDOMAIN, SERVFAIL — so it detects an open resolver even on a cluster
/// with no internet.
pub const PROBE_DNS_NAME: &str = "example.com.";

/// A probe Pod must be done well within this — it is also the Pod's
/// `activeDeadlineSeconds`, so a probe abandoned by a crashed manager cleans
/// itself up.
pub const PROBE_POD_DEADLINE_SECONDS: i64 = 300;

/// How long a Plugin Pod's self-check keeps retrying before it fails the
/// Pod's start. Long enough for a CNI to program a new Pod; a Pod whose
/// network stays open past it never runs its Plugin. The runtime reports the
/// workload failing from that first failed run (the kubelet then restarts
/// the check, with backoff, and it may yet pass).
pub const SELF_CHECK_DEADLINE_SECONDS: u32 = 120;

/// How long the namespace probe's plugin-labelled Pod keeps repeating its
/// checks for the CNI to enforce the policy on it. Shorter than the
/// self-check's: the probe also runs the negative case (an open namespace)
/// to this deadline, and both its Pods must finish within the runtime's
/// wait for them (one more pass of up to ~35 s past the deadline included).
pub const PROBE_ENFORCEMENT_DEADLINE_SECONDS: u32 = 60;

/// The self-check initContainer's name, as `workload_state` looks for it.
pub const SELF_CHECK_CONTAINER: &str = "isolation-self-check";

const LINE_PREFIX: &str = "SCARAB_PROBE";

/// What the probe checks. The `wire` name is what the Pod prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProbeCheck {
    /// Control Pod → canary. MUST be reached.
    ControlReachesCanary,
    /// Control Pod → cluster DNS. MUST answer.
    ControlResolves,
    /// Plugin-labelled Pod → broker healthz. MUST be reached.
    Broker,
    /// Plugin-labelled Pod → any DNS answer. MUST NOT be reached.
    Dns,
    /// Plugin-labelled Pod → canary. MUST NOT be reached.
    Canary,
    /// Plugin-labelled Pod → the API server. MUST NOT be reached.
    ApiServer,
    /// Plugin-labelled Pod → 1.1.1.1:443. MUST NOT be reached.
    Internet,
    /// Plugin-labelled Pod → 169.254.169.254:80. MUST NOT be reached.
    Metadata,
}

impl ProbeCheck {
    fn wire(self) -> &'static str {
        match self {
            ProbeCheck::ControlReachesCanary | ProbeCheck::Canary => "canary",
            ProbeCheck::ControlResolves | ProbeCheck::Dns => "dns",
            ProbeCheck::Broker => "broker",
            ProbeCheck::ApiServer => "apiserver",
            ProbeCheck::Internet => "internet",
            ProbeCheck::Metadata => "metadata",
        }
    }

    fn must_reach(self) -> bool {
        matches!(
            self,
            ProbeCheck::ControlReachesCanary | ProbeCheck::ControlResolves | ProbeCheck::Broker
        )
    }

    /// What a deviation means, for the operator reading `runtime_message`.
    fn deviation(self) -> &'static str {
        match self {
            ProbeCheck::ControlReachesCanary => {
                "inconclusive: the control Pod, whose egress to the canary is allowed, could not \
                 reach it — a blocked plugin connection would prove nothing"
            }
            ProbeCheck::ControlResolves => {
                "inconclusive: the control Pod, whose DNS is allowed, got no DNS answer — a \
                 plugin-side DNS failure would prove nothing"
            }
            ProbeCheck::Broker => {
                "a plugin cannot reach the broker's healthz — the plugin-egress allowance does not \
                 work (check SCARAB_PLUGIN_BROKER_URL / SCARAB_PLUGIN_BROKER_PEER)"
            }
            ProbeCheck::Dns => {
                "a plugin-labelled Pod got a DNS answer — the cluster resolver forwards any name \
                 upstream, so DNS is a tunnel out"
            }
            ProbeCheck::Canary => {
                "NetworkPolicy is NOT enforced: a plugin-labelled Pod reached another Pod (the \
                 canary)"
            }
            ProbeCheck::ApiServer => "a plugin-labelled Pod reached the Kubernetes API server",
            ProbeCheck::Internet => "a plugin-labelled Pod reached 1.1.1.1:443 (the internet)",
            ProbeCheck::Metadata => {
                "a plugin-labelled Pod reached the cloud metadata service (169.254.169.254:80)"
            }
        }
    }
}

const CONTROL_SIDE_CHECKS: [ProbeCheck; 2] = [
    ProbeCheck::ControlReachesCanary,
    ProbeCheck::ControlResolves,
];

const PLUGIN_SIDE_CHECKS: [ProbeCheck; 6] = [
    ProbeCheck::Broker,
    ProbeCheck::Dns,
    ProbeCheck::Canary,
    ProbeCheck::ApiServer,
    ProbeCheck::Internet,
    ProbeCheck::Metadata,
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
/// last; anything else — including `unknown`, a check that could not tell —
/// is ignored, so it reads as never reported.
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
            _ => seen.remove(check),
        };
    }
    seen
}

/// Judge the two probe logs. Every check must report and come out as
/// expected; a missing report is a failure (the Pod did not finish its
/// script, or a check could not tell), never a pass.
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
    for check in CONTROL_SIDE_CHECKS {
        judge(check, &control, "control");
    }
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

/// The shell functions both the namespace probe and every Plugin Pod's
/// self-check run — one script, so the two cannot drift:
///
/// - `reach <check> <host> <port>`: a TCP connect, 3 s, nothing sent;
/// - `resolves <check> <name>`: did ANY DNS answer come back? busybox
///   `nslookup` prints `Name:` for an address, `can't find` for NXDOMAIN /
///   SERVFAIL — both mean a resolver answered — and `no servers could be
///   reached` / `Connection refused` / `unreachable` when nothing did.
///   Anything else is `unknown`, which never counts as a pass;
/// - `broker_ok`: the broker's healthz says `ok` (`$BROKER_URL`);
/// - `plugin_checks`: one pass of the plugin-side checks — the broker
///   reached; DNS, the API server, the internet, the metadata service and,
///   in the namespace probe (the only place `$CANARY_IP` is set), the
///   canary not. An API server address the kubelet did not inject reports
///   `unknown`: a check that could not run is a failure, not a pass;
/// - `isolated <pass>`: was that pass as required — the broker `reached`,
///   every forbidden check positively `unreached` (`unknown` is not);
/// - `until_isolated <seconds>`: repeat `plugin_checks` until a pass is
///   `isolated` (exit 0) or `<seconds>` are up (exit 1), and print the LAST
///   pass — the verdict lines. Repeating, because a CNI programs a new
///   Pod's policy some moments after the Pod starts; within one pass,
///   because a pass is one consistent look at the Pod's network.
pub fn plugin_check_fns() -> String {
    let (internet_host, internet_port) = PROBE_INTERNET_ADDR;
    let (metadata_host, metadata_port) = PROBE_METADATA_ADDR;
    format!(
        r#"reach() {{ if nc -z -w 3 "$2" "$3" >/dev/null 2>&1; then echo "SCARAB_PROBE $1 reached"; else echo "SCARAB_PROBE $1 unreached"; fi; }}
resolves() {{
  out="$(timeout 15 nslookup "$2" 2>&1)"
  case "$out" in
    *"Name:"*|*"can't find"*) echo "SCARAB_PROBE $1 reached" ;;
    *"no servers could be reached"*|*"Connection refused"*|*"unreachable"*|*"can't connect"*) echo "SCARAB_PROBE $1 unreached" ;;
    *) echo "SCARAB_PROBE $1 unknown" ;;
  esac
}}
broker_ok() {{ [ "$(wget -q -T 5 -O - "$BROKER_URL/_scarab/plugin/v1/healthz" 2>/dev/null)" = ok ]; }}
plugin_checks() {{
  if broker_ok; then echo "SCARAB_PROBE broker reached"; else echo "SCARAB_PROBE broker unreached"; fi
  resolves dns {dns_name}
  if [ -z "$KUBERNETES_SERVICE_HOST" ]; then echo "SCARAB_PROBE apiserver unknown"; else reach apiserver "$KUBERNETES_SERVICE_HOST" "$KUBERNETES_SERVICE_PORT"; fi
  reach internet {internet_host} {internet_port}
  reach metadata {metadata_host} {metadata_port}
  if [ -n "$CANARY_IP" ]; then reach canary "$CANARY_IP" {canary_port}; fi
}}
isolated() {{
  case "$1" in *"SCARAB_PROBE broker reached"*) ;; *) return 1 ;; esac
  for c in dns apiserver internet metadata ${{CANARY_IP:+canary}}; do
    case "$1" in *"SCARAB_PROBE $c unreached"*) ;; *) return 1 ;; esac
  done
}}
until_isolated() {{
  until_deadline=$(( $(date +%s) + $1 ))
  while :; do
    out="$(plugin_checks)"
    if isolated "$out"; then echo "$out"; return 0; fi
    if [ "$(date +%s)" -ge "$until_deadline" ]; then echo "$out"; return 1; fi
    sleep 3
  done
}}
"#,
        dns_name = PROBE_DNS_NAME,
        canary_port = PROBE_CANARY_PORT,
    )
}

/// The control Pod: retries the canary for a while — it may have only just
/// become Ready, and a CNI may take a moment to program a new Pod — then
/// retries until it gets a DNS answer (its policy allows DNS), proving the
/// plugin side's "no answer" means something. Both are ALLOWED checks, so
/// both retry until they succeed: a moment of a half-programmed Pod must not
/// read as "the allowance does not work". (It has `CANARY_IP`, but never
/// runs `plugin_checks`: its canary check is the ALLOWED one, above.)
pub fn control_pod(namespace: &str, nonce: &str, image: &str, canary_ip: &str) -> Pod {
    let script = format!(
        r#"{fns}
canary=unreached; i=0
while [ $i -lt 15 ]; do if nc -z -w 3 "$CANARY_IP" {port} >/dev/null 2>&1; then canary=reached; break; fi; i=$((i+1)); sleep 2; done
echo "SCARAB_PROBE canary $canary"
i=0
while :; do dns="$(resolves dns {dns_name})"; if [ "$dns" = "SCARAB_PROBE dns reached" ] || [ $i -ge 4 ]; then break; fi; i=$((i+1)); sleep 2; done
echo "$dns"
"#,
        fns = plugin_check_fns(),
        port = PROBE_CANARY_PORT,
        dns_name = PROBE_DNS_NAME,
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
/// It runs `until_isolated` — the self-check's own loop — with `$CANARY_IP`
/// set, so the canary is one more forbidden check of every pass. A pass
/// right after the Pod starts can run before the CNI has programmed it
/// (everything reached); the loop waits that out, up to
/// [`PROBE_ENFORCEMENT_DEADLINE_SECONDS`], and prints its last pass — which,
/// on a CNI that never enforces, is the open one, and fails the probe.
/// (The unmeasured first moments of a REAL Plugin Pod are the self-check's.)
///
/// `broker_url` is the URL as a Pod dials it — the broker's IP, since a
/// Plugin has no DNS.
pub fn plugin_probe_pod(
    namespace: &str,
    nonce: &str,
    image: &str,
    canary_ip: &str,
    broker_url: &str,
) -> Pod {
    let mut labels = probe_labels(nonce, PROBE_ROLE_PLUGIN);
    labels.insert(LABEL_PLUGIN_WORKLOAD.to_string(), "true".to_string());
    // The exit status is not the verdict: the runtime [`assess`]es the
    // printed pass, so a missing line fails closed as well.
    let script = format!(
        r#"{fns}
until_isolated {deadline}
"#,
        fns = plugin_check_fns(),
        deadline = PROBE_ENFORCEMENT_DEADLINE_SECONDS,
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
            ]),
            ..Default::default()
        },
    )
}

/// The self-check script: [`plugin_check_fns`]' `until_isolated`, with
/// [`SELF_CHECK_DEADLINE_SECONDS`]. Exit 0 lets the Plugin start; exit 1
/// keeps it from starting, with the last pass's verdict lines as the
/// termination message.
pub fn self_check_script() -> String {
    format!(
        r#"{fns}
if until_isolated {deadline}; then echo "isolation self-check passed"; exit 0; fi
echo "isolation self-check FAILED: this Pod's network is not isolated as plugin-egress requires"
exit 1
"#,
        fns = plugin_check_fns(),
        deadline = SELF_CHECK_DEADLINE_SECONDS,
    )
}

/// The Plugin Pod's self-check initContainer (ADR-0072 §9): the namespace
/// probe proves ONE node, once, for its own probe Pod; this proves the
/// Plugin's own Pod, on its own node, before the Plugin runs. It gets the
/// broker URL and nothing else — no config, no token (and no `CANARY_IP`,
/// so it never checks a canary that is not there).
pub fn self_check_container(image: &str, broker_url: &str) -> Container {
    let quantity = |q: &str| Quantity(q.to_string());
    Container {
        name: SELF_CHECK_CONTAINER.to_string(),
        image: Some(image.to_string()),
        image_pull_policy: Some("IfNotPresent".to_string()),
        command: Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            self_check_script(),
        ]),
        env: Some(vec![env("BROKER_URL", broker_url.trim_end_matches('/'))]),
        security_context: Some(restricted_container_security_context()),
        // The verdict lines become the termination message the runtime
        // shows when the check keeps failing.
        termination_message_policy: Some("FallbackToLogsOnError".to_string()),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), quantity("10m")),
                ("memory".to_string(), quantity("16Mi")),
            ])),
            limits: Some(BTreeMap::from([
                (
                    "cpu".to_string(),
                    quantity(crate::objects::PLUGIN_CPU_LIMIT),
                ),
                ("memory".to_string(), quantity("64Mi")),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTROL_OK: &str = "SCARAB_PROBE canary reached\nSCARAB_PROBE dns reached\n";
    const PLUGIN_OK: &str = "SCARAB_PROBE broker reached\nSCARAB_PROBE dns unreached\n\
        SCARAB_PROBE apiserver unreached\nSCARAB_PROBE internet unreached\n\
        SCARAB_PROBE metadata unreached\nSCARAB_PROBE canary unreached\n";

    fn script_of(pod: &Pod) -> String {
        pod.spec.as_ref().unwrap().containers[0]
            .command
            .clone()
            .unwrap()[2]
            .clone()
    }

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
        for check in ["dns", "apiserver", "internet", "metadata"] {
            let leaky =
                PLUGIN_OK.replace(&format!("{check} unreached"), &format!("{check} reached"));
            assert!(!assess(CONTROL_OK, &leaky).passed(), "{check}");
        }
        // DNS in particular: any answer is a tunnel out.
        let resolving = PLUGIN_OK.replace("dns unreached", "dns reached");
        assert!(assess(CONTROL_OK, &resolving)
            .message()
            .contains("DNS is a tunnel out"));
    }

    #[test]
    fn a_broken_allowance_fails_the_probe() {
        let broken = PLUGIN_OK.replace("broker reached", "broker unreached");
        assert!(!assess(CONTROL_OK, &broken).passed());
    }

    #[test]
    fn a_control_that_cannot_do_what_it_is_allowed_makes_the_probe_inconclusive() {
        for control in [
            "SCARAB_PROBE canary unreached\nSCARAB_PROBE dns reached\n",
            // A plugin-side "no DNS answer" means nothing if the control,
            // whose DNS is allowed, gets none either.
            "SCARAB_PROBE canary reached\nSCARAB_PROBE dns unreached\n",
        ] {
            let report = assess(control, PLUGIN_OK);
            assert!(!report.passed());
            assert!(report.message().contains("inconclusive"), "{report:?}");
        }
    }

    #[test]
    fn a_check_that_never_reported_or_could_not_tell_is_a_failure() {
        let truncated: String = PLUGIN_OK.lines().take(2).collect::<Vec<_>>().join("\n");
        let report = assess(CONTROL_OK, &truncated);
        assert!(!report.passed());
        assert!(report.message().contains("never reported its `apiserver`"));
        // `unknown` (no KUBERNETES_SERVICE_HOST, an nslookup that said
        // nothing recognisable) is not a pass — even after an earlier line.
        for check in ["apiserver", "dns"] {
            let unknown = format!("{PLUGIN_OK}SCARAB_PROBE {check} unknown\n");
            assert!(!assess(CONTROL_OK, &unknown).passed(), "{check}");
        }
        assert!(!assess("", "").passed());
        assert!(!ProbeReport::could_not_run("timeout").passed());
    }

    #[test]
    fn noise_in_the_log_is_ignored() {
        let noisy = format!("wget: bad address 'x'\nSCARAB_PROBE bogus\n{PLUGIN_OK}");
        assert!(assess(CONTROL_OK, &noisy).passed());
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
            "http://10.96.0.20:8090",
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
    fn the_plugin_side_script_checks_every_destination() {
        let plugin = plugin_probe_pod("ns", "n1", "img", "10.0.0.9", "http://10.96.0.20:8090");
        let script = script_of(&plugin);
        for needle in [
            "plugin_checks",
            "SCARAB_PROBE broker",
            "resolves dns example.com.",
            "reach apiserver \"$KUBERNETES_SERVICE_HOST\"",
            "reach internet 1.1.1.1 443",
            "reach metadata 169.254.169.254 80",
            "reach canary \"$CANARY_IP\" 8080",
            // Every forbidden destination, the canary included where it is
            // set, must be positively unreached in one pass.
            "for c in dns apiserver internet metadata ${CANARY_IP:+canary}",
        ] {
            assert!(script.contains(needle), "{needle} missing from {script}");
        }
        // The forbidden checks are repeated until enforced, not judged once:
        // a CNI programs a new Pod's policy only after it starts.
        assert!(
            script.ends_with(&format!(
                "\nuntil_isolated {PROBE_ENFORCEMENT_DEADLINE_SECONDS}\n"
            )),
            "{script}"
        );
        assert!(
            plugin.spec.as_ref().unwrap().containers[0]
                .env
                .as_ref()
                .unwrap()
                .iter()
                .any(|e| e.name == "CANARY_IP"),
            "the probe's plugin side checks the canary"
        );
        // The control checks its allowed DNS with the very same function.
        let control = script_of(&control_pod("ns", "n1", "img", "10.0.0.9"));
        assert!(control.contains("resolves dns example.com."), "{control}");
    }

    /// The self-check and the namespace probe run one script: what the probe
    /// proves of the namespace is what every Plugin Pod re-proves of itself.
    #[test]
    fn the_self_check_runs_the_probes_plugin_side_checks_until_a_deadline() {
        let probe = script_of(&plugin_probe_pod(
            "ns",
            "n1",
            "img",
            "10.0.0.9",
            "http://10.96.0.20:8090",
        ));
        let init = self_check_container("img", "http://10.96.0.20:8090/");
        let script = init.command.clone().unwrap()[2].clone();
        assert!(probe.starts_with(&plugin_check_fns()));
        assert!(script.starts_with(&plugin_check_fns()));
        // The same loop, to the self-check's own deadline.
        assert!(
            script.contains(&format!(
                "if until_isolated {SELF_CHECK_DEADLINE_SECONDS}; then"
            )),
            "{script}"
        );
        assert!(script.trim_end().ends_with("exit 1"), "{script}");
        assert_eq!(init.name, SELF_CHECK_CONTAINER);
        assert_eq!(
            init.env.unwrap(),
            vec![env("BROKER_URL", "http://10.96.0.20:8090")],
            "the broker URL and nothing else — no config, no token"
        );
        assert!(init.volume_mounts.is_none());
        let csc = init.security_context.unwrap();
        assert_eq!(csc.allow_privilege_escalation, Some(false));
        assert_eq!(csc.read_only_root_filesystem, Some(true));
        let limits = init.resources.unwrap().limits.unwrap();
        assert!(limits.contains_key("cpu") && limits.contains_key("memory"));
    }
}
