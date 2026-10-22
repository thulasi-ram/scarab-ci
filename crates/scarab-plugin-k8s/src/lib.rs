//! # scarab-plugin-k8s — the plugin runtime on Kubernetes
//!
//! The adapter behind ADR-0072 §9 (runtime isolation) and §11 (who is
//! calling the broker):
//!
//! - [`K8sPluginRuntime`] keeps the one shared plugin namespace's **baseline**
//!   — it refuses to run anything unless the namespace enforces the
//!   `restricted` Pod Security level, and server-side-applies a default-deny
//!   NetworkPolicy plus the one egress allowance a Plugin gets (DNS and the
//!   CredentialBroker) — and runs one ServiceAccount + Deployment per
//!   PluginInstallation.
//! - The **isolation probe** ([`K8sPluginRuntime::probe_isolation`]) checks
//!   that the cluster actually enforces that policy before any Plugin starts:
//!   a cluster whose CNI ignores NetworkPolicy fails it, and the Plugin does
//!   not run.
//! - [`TokenReviewVerifier`] proves a broker caller's projected
//!   ServiceAccount token with TokenReview — audience `scarab-broker`, bound
//!   to a live Pod — and caches the verdict briefly. The kill switch is the
//!   broker's own database check on every request, so the cache never
//!   delays a revocation.
//!
//! Object shapes are built by pure functions ([`objects`], [`probe`]) and
//! unit-tested without a cluster; the live tier is `tests/cluster.rs`.

use std::collections::BTreeMap;
use std::net::IpAddr;

pub mod objects;
pub mod probe;
mod runtime;
mod token_review;

pub use probe::{ProbeReport, ProbeVerdict};
pub use runtime::{K8sPluginRuntime, PluginRuntimeConfig, RuntimeError, WorkloadState};
pub use token_review::{TokenReviewVerifier, VERDICT_CACHE_TTL};

/// A client for the cluster this process is configured for: in-cluster
/// config in a Pod, `KUBECONFIG` (the isolated kind kubeconfig) in proc mode.
/// The composition root holds it without naming kube's types.
pub async fn default_client() -> Result<kube::Client, String> {
    kube::Client::try_default().await.map_err(|e| e.to_string())
}

/// The field manager every object this runtime applies is owned by.
pub const FIELD_MANAGER: &str = "scarab-plugin-runtime";

/// What `plugin-egress` selects: every Plugin workload — and the probe's
/// plugin-labelled Pod, which must be treated exactly like one.
pub const LABEL_PLUGIN_WORKLOAD: &str = "scarab.io/plugin-workload";
/// The installation id an object belongs to. The reconcile loop lists
/// Deployments by it to find workloads whose installation is gone.
pub const LABEL_INSTALLATION: &str = "scarab.io/plugin-installation";
/// `app.kubernetes.io/managed-by` on everything this runtime creates.
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY: &str = "scarab";

/// The Pod Security Admission label the plugin namespace must carry, and the
/// level it must name (ADR-0072 §9). The runtime only reads it: who may
/// label namespaces is the operator's (the chart, or `deploy/local-proc/up.sh`).
pub const PSA_ENFORCE_LABEL: &str = "pod-security.kubernetes.io/enforce";
pub const PSA_RESTRICTED: &str = "restricted";

/// How a plugin NetworkPolicy names the CredentialBroker
/// (`SCARAB_PLUGIN_BROKER_PEER`). In-cluster (Helm) the broker is the
/// control plane's own Pods; in proc mode it is a host process, reachable
/// only by address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerPeer {
    /// `pods:<namespace>:<k=v,…>` — Pods with these labels in that namespace.
    Pods {
        namespace: String,
        labels: BTreeMap<String, String>,
    },
    /// `cidr:<cidr>` — an address block, at most a /24 (IPv4) or /120
    /// (IPv6).
    Cidr(String),
}

/// The widest `cidr:` broker peer accepted. A broker is one host or a
/// handful; a wide block would quietly turn the one egress allowance into
/// "most of the network", which the probe can only partly notice.
pub const MIN_BROKER_CIDR_PREFIX_V4: u8 = 24;
pub const MIN_BROKER_CIDR_PREFIX_V6: u8 = 120;

impl BrokerPeer {
    /// Parse `pods:<namespace>:<k=v,…>` or `cidr:<ip>/<prefix>`.
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(rest) = s.strip_prefix("pods:") {
            let (namespace, selector) = rest
                .split_once(':')
                .ok_or_else(|| format!("`{s}`: want `pods:<namespace>:<k=v,…>`"))?;
            if scarab_plugin::validate_installation_name(namespace).is_err() {
                return Err(format!("`{s}`: `{namespace}` is not a namespace name"));
            }
            let mut labels = BTreeMap::new();
            for pair in selector.split(',').filter(|p| !p.is_empty()) {
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("`{s}`: label `{pair}` is not `key=value`"))?;
                if k.is_empty() || !is_label_text(k) || !is_label_text(v) {
                    return Err(format!("`{s}`: label `{pair}` is not a valid label"));
                }
                labels.insert(k.to_string(), v.to_string());
            }
            if labels.is_empty() {
                // An empty selector is "every Pod in the namespace" — never
                // what a broker peer means.
                return Err(format!("`{s}`: name at least one label"));
            }
            return Ok(BrokerPeer::Pods {
                namespace: namespace.to_string(),
                labels,
            });
        }
        if let Some(cidr) = s.strip_prefix("cidr:") {
            let (ip, prefix) = cidr
                .split_once('/')
                .ok_or_else(|| format!("`{s}`: want `cidr:<ip>/<prefix>`"))?;
            let ip: IpAddr = ip
                .parse()
                .map_err(|_| format!("`{s}`: `{ip}` is not an IP address"))?;
            let prefix: u8 = prefix
                .parse()
                .map_err(|_| format!("`{s}`: `{prefix}` is not a prefix length"))?;
            let (min, max) = match ip {
                IpAddr::V4(_) => (MIN_BROKER_CIDR_PREFIX_V4, 32),
                IpAddr::V6(_) => (MIN_BROKER_CIDR_PREFIX_V6, 128),
            };
            if prefix > max {
                return Err(format!("`{s}`: prefix /{prefix} is longer than /{max}"));
            }
            if prefix < min {
                return Err(format!(
                    "`{s}`: /{prefix} is wider than /{min}; a broker peer names the broker, \
                     not a network (ADR-0072 §9)"
                ));
            }
            return Ok(BrokerPeer::Cidr(cidr.to_string()));
        }
        Err(format!(
            "`{s}`: want `pods:<namespace>:<k=v,…>` or `cidr:<ip>/<prefix>`"
        ))
    }
}

/// Characters a label key or value may hold (the key may carry a
/// `prefix/`). Shape only; the API server has the last word.
fn is_label_text(s: &str) -> bool {
    s.len() <= 316
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pods_peer_names_a_namespace_and_labels() {
        assert_eq!(
            BrokerPeer::parse(
                "pods:scarab:app.kubernetes.io/name=scarab,app.kubernetes.io/instance=prod"
            ),
            Ok(BrokerPeer::Pods {
                namespace: "scarab".into(),
                labels: BTreeMap::from([
                    ("app.kubernetes.io/instance".into(), "prod".into()),
                    ("app.kubernetes.io/name".into(), "scarab".into()),
                ]),
            })
        );
    }

    #[test]
    fn a_cidr_peer_is_a_host_or_a_small_block() {
        assert_eq!(
            BrokerPeer::parse("cidr:192.168.5.2/32"),
            Ok(BrokerPeer::Cidr("192.168.5.2/32".into()))
        );
        assert!(BrokerPeer::parse("cidr:fd00::1/128").is_ok());
    }

    #[test]
    fn a_peer_that_would_widen_the_allowance_is_refused() {
        for bad in [
            // every Pod of the namespace
            "pods:scarab:",
            // every destination
            "cidr:0.0.0.0/0",
            "cidr:10.0.0.0/8",
            "cidr:fd00::/64",
            // shapes
            "pods:Scarab:app=x",
            "pods:scarab",
            "pods:scarab:app",
            "cidr:192.168.5.2",
            "cidr:192.168.5.2/33",
            "cidr:not-an-ip/32",
            "host:192.168.5.2",
        ] {
            assert!(BrokerPeer::parse(bad).is_err(), "{bad} must be refused");
        }
    }
}
