//! # scarab-plugin-k8s — the plugin runtime on Kubernetes
//!
//! The adapter behind ADR-0072 §9 (runtime isolation) and §11 (who is
//! calling the broker):
//!
//! - [`K8sPluginRuntime`] keeps the one shared plugin namespace's **baseline**
//!   — it refuses to run anything unless the namespace enforces the
//!   `restricted` Pod Security level, and server-side-applies a default-deny
//!   NetworkPolicy plus the one egress allowance a Plugin gets (the
//!   CredentialBroker, by IP — no DNS) — and runs one ServiceAccount +
//!   Deployment per PluginInstallation, each Pod gated by an isolation
//!   self-check initContainer.
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
///
/// The client speaks rustls, and the binaries that link this crate link more
/// than one rustls CryptoProvider, so none is auto-selected: the caller must
/// have installed one before the first request. The only non-test caller,
/// `scarab-server`'s `main`, installs `ring` before anything else runs
/// (`main.rs`, ahead of `Cli::parse`); the live tier does the same in
/// `tests/cluster.rs`. A new caller must too, or its first TLS handshake
/// panics.
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

/// `SCARAB_PLUGIN_BROKER_URL`, taken apart: what a Plugin Pod dials to reach
/// the broker. A Plugin has no DNS (its egress allows the broker and nothing
/// else), so a URL naming a HOST is resolved by the runtime — which can — and
/// handed to Pods by IP ([`BrokerUrl::with_ip`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerUrl {
    scheme: String,
    host: String,
    port: u16,
    /// `""` or `/…`, with no trailing `/`.
    path: String,
}

impl BrokerUrl {
    /// `http(s)://<host>[:<port>][/<path>]`, no credentials, no query.
    pub fn parse(url: &str) -> Result<Self, String> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| format!("`{url}`: want `http://<host>:<port>`"))?;
        let default_port = match scheme {
            "http" => 80,
            "https" => 443,
            _ => return Err(format!("`{url}`: the scheme must be http or https")),
        };
        if rest.contains(['@', '?', '#']) {
            return Err(format!(
                "`{url}`: no credentials, query or fragment in a broker URL"
            ));
        }
        let (authority, path) = match rest.find('/') {
            Some(at) => rest.split_at(at),
            None => (rest, ""),
        };
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (host, after) = bracketed
                .split_once(']')
                .ok_or_else(|| format!("`{url}`: unclosed `[`"))?;
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(format!("`{url}`: `[{host}]` is not an IPv6 address"));
            }
            (host, after.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(format!("`{url}`: no host"));
        }
        let port = match port {
            None => default_port,
            Some(p) => p
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| format!("`{url}`: `{p}` is not a port"))?,
        };
        Ok(BrokerUrl {
            scheme: scheme.to_string(),
            host: host.to_string(),
            port,
            path: path.trim_end_matches('/').to_string(),
        })
    }

    /// The host, as written (a name or an IP literal, unbracketed).
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The host when it is already an IP: then there is nothing to resolve.
    pub fn ip(&self) -> Option<IpAddr> {
        self.host.parse().ok()
    }

    /// The same URL with the host replaced by `ip`. (An `https` URL by IP
    /// cannot verify a certificate issued to a name — the broker hop is
    /// plain HTTP inside the cluster today; revisit if it ever is not.)
    pub fn with_ip(&self, ip: IpAddr) -> String {
        let host = match ip {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("[{v6}]"),
        };
        format!("{}://{host}:{}{}", self.scheme, self.port, self.path)
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
    fn a_broker_url_by_name_is_rewritten_to_the_ip_a_pod_dials() {
        let url = BrokerUrl::parse("http://scarab-broker.scarab.svc:8090/").unwrap();
        assert_eq!(url.host(), "scarab-broker.scarab.svc");
        assert_eq!(url.port(), 8090);
        assert_eq!(url.ip(), None);
        assert_eq!(
            url.with_ip("10.96.0.20".parse().unwrap()),
            "http://10.96.0.20:8090"
        );
        assert_eq!(
            url.with_ip("fd00::20".parse().unwrap()),
            "http://[fd00::20]:8090"
        );
        // Already an IP: nothing to resolve.
        let by_ip = BrokerUrl::parse("http://192.168.5.2:8090").unwrap();
        assert_eq!(by_ip.ip(), Some("192.168.5.2".parse().unwrap()));
        assert_eq!(
            BrokerUrl::parse("http://[fd00::1]:8090").unwrap().ip(),
            Some("fd00::1".parse().unwrap())
        );
        // Default port, and a path kept.
        assert_eq!(
            BrokerUrl::parse("http://broker/base/")
                .unwrap()
                .with_ip("10.0.0.1".parse().unwrap()),
            "http://10.0.0.1:80/base"
        );
    }

    #[test]
    fn a_broker_url_that_is_not_one_is_refused() {
        for bad in [
            "scarab-broker:8090",
            "ftp://broker:8090",
            "http://:8090",
            "http://broker:0",
            "http://broker:http",
            "http://user:pw@broker:8090",
            "http://broker:8090/?x=1",
            "http://[fd00::1:8090",
            "http://[not-v6]:8090",
        ] {
            assert!(BrokerUrl::parse(bad).is_err(), "{bad} must be refused");
        }
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
