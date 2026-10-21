//! # scarab-plugin — the governed extension point beside Steps
//!
//! Pure domain crate (CONTEXT §4.7, ADR-0072). A **Plugin** is a
//! digest-pinned OCI image plus a [`PluginManifest`]; an org's approval to
//! run one pinned digest of it is a [`PluginInstallation`], which acts under
//! its own [`PluginPrincipal`]. What a Plugin may do is a **closed**
//! vocabulary ([`PluginCapability`]) that grows only with a Scarab release,
//! and what this release can actually run is narrower still
//! ([`admit_for_mmp`]).
//!
//! This crate decides shapes and rules only. Running the Plugin
//! (Kubernetes), proving who is calling (TokenReview) and storing anything
//! (Postgres) are adapters behind [`PluginTokenVerifier`] and
//! [`PluginStore`].
//!
//! ## Rules
//!
//! - **The manifest is the approval.** An org admin approves exactly
//!   *(image digest, manifest)*; nothing changes without another admin
//!   action (ADR-0072 §10). A manifest with a field this release does not
//!   understand is refused, never partly read.
//! - **Core sets the source.** A report never names its source; the broker
//!   sets it from the verified ServiceAccount's installation (ADR-0072 §11).
//!   [`ReportItem`] has no such field and refuses unknown ones.
//! - **Transitions, in source order** (ADR-0070 §11). Each report carries an
//!   idempotency key and a sequence per *(installation, DeploymentTarget)*;
//!   a repeat is dropped and a sequence that is not newer is stale.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

mod report;
mod store;

pub use report::{
    OwnershipReportItem, ReportBatch, ReportIngest, ReportItem, ReportItemOutcome, ReportOutcome,
    ReportedStatus, StatusReportItem, MAX_IDEMPOTENCY_KEY_CHARS, MAX_OWNER_PROJECTS,
    MAX_REPORT_ITEMS, MAX_REPORT_LABEL_CHARS, MAX_REPORT_LINK_CHARS, MAX_TARGET_ID_CHARS,
    MAX_VERSION_CHARS, MAX_VERSION_SCHEME_CHARS,
};
pub use store::{
    DeploymentSourceTarget, InstallationDeploymentSource, PluginStore, PluginStoreError,
    UnmappedGrouping, REPORT_RECEIPT_RETENTION_MS,
};

/// The longest installation or plugin name: a DNS-1123 label. Installation
/// names become `DeploymentTarget.source` and plugin names a catalog key;
/// both name Kubernetes objects in slice (d2).
pub use scarab_project::MAX_INSTALLATION_NAME_CHARS;

/// How many hex characters a server-minted installation id has.
pub const INSTALLATION_ID_HEX_CHARS: usize = 10;

/// The most connection slots, read rules per slot, and config keys a manifest
/// may declare: generous for any real vendor API, small enough that an
/// approval screen can show all of it.
pub const MAX_CONNECTION_SLOTS: usize = 16;
pub const MAX_READ_RULES_PER_SLOT: usize = 64;
pub const MAX_CONFIG_KEYS: usize = 32;
/// The longest read-rule path pattern and config value, in characters.
pub const MAX_RULE_PATH_CHARS: usize = 512;
pub const MAX_CONFIG_VALUE_CHARS: usize = 2048;

/// One entry of the **closed** PluginCapability vocabulary (ADR-0072 §2).
/// Every variant exists so a manifest naming it parses and can be refused by
/// name; which ones this release runs is [`admit_for_mmp`]'s call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginCapability {
    /// DeploymentStatusReports and DeploymentTargetProjectOwnership for the
    /// sources an Environment admits (ADR-0071 §5, ADR-0070 §11).
    ReportDeployments,
    /// Read-only Run and Deployment events.
    SubscribeEvents,
    /// Declare PluginActions humans or Runs may invoke (ADR-0072 §4).
    OfferActions,
    /// Reach a named external system through the CredentialBroker without
    /// holding its credential (ADR-0072 §5).
    UseConnection,
    /// A PluginUiContribution at a declared trust tier (ADR-0072 §7).
    ContributeUi,
    /// Approve or reject approval gates where an Environment admin names the
    /// PluginPrincipal (ADR-0072 §3).
    ApproveGates,
}

impl PluginCapability {
    /// Every capability, in declaration order.
    pub const ALL: [PluginCapability; 6] = [
        PluginCapability::ReportDeployments,
        PluginCapability::SubscribeEvents,
        PluginCapability::OfferActions,
        PluginCapability::UseConnection,
        PluginCapability::ContributeUi,
        PluginCapability::ApproveGates,
    ];

    /// The wire name (`report_deployments`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            PluginCapability::ReportDeployments => "report_deployments",
            PluginCapability::SubscribeEvents => "subscribe_events",
            PluginCapability::OfferActions => "offer_actions",
            PluginCapability::UseConnection => "use_connection",
            PluginCapability::ContributeUi => "contribute_ui",
            PluginCapability::ApproveGates => "approve_gates",
        }
    }
}

impl fmt::Display for PluginCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The capabilities this release can run (ADR-0072, MMP horizon): reporting,
/// and connections with **read** rules only. Everything else is refused at
/// install, by name, rather than accepted and ignored.
pub const MMP_ADMITTED_CAPABILITIES: [PluginCapability; 2] = [
    PluginCapability::ReportDeployments,
    PluginCapability::UseConnection,
];

/// An allowed request shape on a PluginConnection (ADR-0072 §5): a method
/// and a path pattern, `*` matching exactly one segment. The manifest
/// declares **read** rules only; write rules belong to PluginActions (MLP).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRule {
    /// `GET` or `HEAD`: a read rule may not change anything.
    pub method: String,
    /// Absolute, `/`-separated; `*` is one whole segment.
    pub path: String,
}

/// A named external system the Plugin reaches through the broker (Argo CD's
/// API, say). The admin binds it to one destination and credential as a
/// PluginConnection (slice d3); the manifest only names the slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionSlot {
    pub name: String,
    /// The schemes the broker may inject. Only `bearer` exists today.
    pub auth_schemes: Vec<String>,
    #[serde(default)]
    pub read_rules: Vec<RequestRule>,
}

/// The auth schemes the broker can inject (ADR-0072 §5).
pub const KNOWN_AUTH_SCHEMES: [&str; 1] = ["bearer"];

/// Everything a Plugin asks for (ADR-0072 §1-2). What an org admin approves
/// is exactly *(image digest, this document)*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// The Plugin's name: a DNS-1123 label, the catalog key.
    pub plugin: String,
    pub capabilities: Vec<PluginCapability>,
    #[serde(default)]
    pub connections: Vec<ConnectionSlot>,
    /// The configuration an installation may set (non-secret; secrets are
    /// PluginConnections). Environment-variable shaped.
    #[serde(default)]
    pub config_keys: Vec<String>,
}

impl PluginManifest {
    /// Does it ask for `capability`?
    pub fn has(&self, capability: PluginCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Check every rule the types do not already guarantee: names, no
    /// duplicates, bounded sizes, read rules that are reads, and connections
    /// declared exactly when `use_connection` is asked for.
    pub fn validate(&self) -> Result<(), PluginError> {
        let invalid = |m: String| Err(PluginError::InvalidManifest(m));
        if !is_dns1123_label(&self.plugin) {
            return invalid(format!(
                "plugin name `{}` must be a DNS-1123 label (1-{MAX_INSTALLATION_NAME_CHARS} \
                 lowercase letters, digits or `-`)",
                self.plugin
            ));
        }
        if self.capabilities.is_empty() {
            return invalid("a plugin must ask for at least one capability".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for capability in &self.capabilities {
            if !seen.insert(capability) {
                return invalid(format!("capability `{capability}` is listed twice"));
            }
        }
        // A slot without the capability would be a grant nobody approved by
        // name; the capability without a slot names nothing to reach.
        match (
            self.has(PluginCapability::UseConnection),
            self.connections.is_empty(),
        ) {
            (true, true) => {
                return invalid("`use_connection` needs at least one connection slot".into())
            }
            (false, false) => {
                return invalid("connection slots need the `use_connection` capability".into())
            }
            _ => {}
        }
        if self.connections.len() > MAX_CONNECTION_SLOTS {
            return invalid(format!(
                "at most {MAX_CONNECTION_SLOTS} connection slots, got {}",
                self.connections.len()
            ));
        }
        let mut slots = std::collections::BTreeSet::new();
        for slot in &self.connections {
            slot.validate()?;
            if !slots.insert(slot.name.as_str()) {
                return invalid(format!("connection slot `{}` is listed twice", slot.name));
            }
        }
        if self.config_keys.len() > MAX_CONFIG_KEYS {
            return invalid(format!(
                "at most {MAX_CONFIG_KEYS} config keys, got {}",
                self.config_keys.len()
            ));
        }
        let mut keys = std::collections::BTreeSet::new();
        for key in &self.config_keys {
            if !is_config_key(key) {
                return invalid(format!(
                    "config key `{key}` must be 1-64 of `A-Z`, `0-9`, `_`, not starting \
                     with a digit"
                ));
            }
            if !keys.insert(key.as_str()) {
                return invalid(format!("config key `{key}` is listed twice"));
            }
        }
        Ok(())
    }

    /// The installation `config` checked against the declared keys: an
    /// undeclared key is refused (it would be configuration nobody approved),
    /// and values are bounded, single-line text.
    pub fn validate_config(&self, config: &BTreeMap<String, String>) -> Result<(), PluginError> {
        for (key, value) in config {
            if !self.config_keys.iter().any(|k| k == key) {
                return Err(PluginError::InvalidConfig(format!(
                    "`{key}` is not a config key plugin `{}` declares",
                    self.plugin
                )));
            }
            if value.chars().count() > MAX_CONFIG_VALUE_CHARS {
                return Err(PluginError::InvalidConfig(format!(
                    "`{key}` is longer than {MAX_CONFIG_VALUE_CHARS} characters"
                )));
            }
            if value.chars().any(char::is_control) {
                return Err(PluginError::InvalidConfig(format!(
                    "`{key}` contains a control character"
                )));
            }
        }
        Ok(())
    }
}

impl ConnectionSlot {
    fn validate(&self) -> Result<(), PluginError> {
        let invalid = |m: String| Err(PluginError::InvalidManifest(m));
        if !is_dns1123_label(&self.name) {
            return invalid(format!(
                "connection slot `{}` must be a DNS-1123 label",
                self.name
            ));
        }
        if self.auth_schemes.is_empty() {
            return invalid(format!(
                "connection slot `{}` needs an auth scheme",
                self.name
            ));
        }
        for scheme in &self.auth_schemes {
            if !KNOWN_AUTH_SCHEMES.contains(&scheme.as_str()) {
                return invalid(format!(
                    "connection slot `{}`: unknown auth scheme `{scheme}` (known: {})",
                    self.name,
                    KNOWN_AUTH_SCHEMES.join(", ")
                ));
            }
        }
        if self.read_rules.len() > MAX_READ_RULES_PER_SLOT {
            return invalid(format!(
                "connection slot `{}`: at most {MAX_READ_RULES_PER_SLOT} read rules",
                self.name
            ));
        }
        for rule in &self.read_rules {
            rule.validate_read(&self.name)?;
        }
        Ok(())
    }
}

impl RequestRule {
    /// A read rule: `GET`/`HEAD`, and a path pattern that cannot be read two
    /// ways — absolute, no empty, `.` or `..` segment, no `%`, `\`, `?` or
    /// `#` (the broker decodes before matching, slice d3, so an encoded
    /// separator in a *pattern* could only be a mistake), and `*` only as a
    /// whole segment.
    fn validate_read(&self, slot: &str) -> Result<(), PluginError> {
        let invalid = |m: String| Err(PluginError::InvalidManifest(m));
        if !matches!(self.method.as_str(), "GET" | "HEAD") {
            return invalid(format!(
                "connection slot `{slot}`: read rule method `{}` is not GET or HEAD",
                self.method
            ));
        }
        let path = &self.path;
        let Some(rest) = path.strip_prefix('/') else {
            return invalid(format!(
                "connection slot `{slot}`: rule path `{path}` must start with `/`"
            ));
        };
        if path.chars().count() > MAX_RULE_PATH_CHARS
            || path
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '%' | '\\' | '?' | '#'))
        {
            return invalid(format!(
                "connection slot `{slot}`: rule path `{}` is too long or has a character \
                 a pattern may not hold",
                path.escape_debug()
            ));
        }
        for segment in rest.split('/') {
            let bad = segment.is_empty()
                || segment == "."
                || segment == ".."
                || (segment.contains('*') && segment != "*");
            if bad {
                return invalid(format!(
                    "connection slot `{slot}`: rule path `{path}` has an empty, `.`/`..` or \
                     partial-`*` segment"
                ));
            }
        }
        Ok(())
    }
}

/// Can this release run a Plugin with `manifest`? Only the MMP's
/// capabilities ([`MMP_ADMITTED_CAPABILITIES`]); anything else is refused by
/// name so the admin learns what is missing, never accepted and ignored.
/// Connections are read-only by construction — a manifest has no write rules
/// — and their rules are re-checked here so admission never depends on a
/// caller having validated first.
pub fn admit_for_mmp(manifest: &PluginManifest) -> Result<(), PluginError> {
    manifest.validate()?;
    let refused: Vec<&str> = manifest
        .capabilities
        .iter()
        .filter(|c| !MMP_ADMITTED_CAPABILITIES.contains(c))
        .map(|c| c.as_str())
        .collect();
    if !refused.is_empty() {
        return Err(PluginError::CapabilityNotAdmitted(refused.join(", ")));
    }
    Ok(())
}

/// Whether an installation's reports and broker calls are accepted. The kill
/// switch (ADR-0072 §10): `Killed` stops both at once, without an upgrade or
/// uninstall, and revokes at the token-to-installation mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstallationState {
    Enabled,
    Killed,
}

impl InstallationState {
    pub fn as_str(self) -> &'static str {
        match self {
            InstallationState::Enabled => "enabled",
            InstallationState::Killed => "killed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "enabled" => Some(InstallationState::Enabled),
            "killed" => Some(InstallationState::Killed),
            _ => None,
        }
    }
}

/// What the plugin runtime (slice d2) last observed of an installation's
/// workload. Reported, never acted on by the broker: whether a call is
/// accepted is [`InstallationState`]'s alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeStatus {
    /// Installed; the runtime has not acted on it yet.
    Pending,
    /// The isolation probe is running (ADR-0072 §9).
    Probing,
    Running,
    /// The isolation probe found NetworkPolicy unenforced; the Plugin was not
    /// started.
    ProbeFailed,
    Error,
    /// Scaled to zero (killed).
    Stopped,
}

impl RuntimeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeStatus::Pending => "pending",
            RuntimeStatus::Probing => "probing",
            RuntimeStatus::Running => "running",
            RuntimeStatus::ProbeFailed => "probe_failed",
            RuntimeStatus::Error => "error",
            RuntimeStatus::Stopped => "stopped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(RuntimeStatus::Pending),
            "probing" => Some(RuntimeStatus::Probing),
            "running" => Some(RuntimeStatus::Running),
            "probe_failed" => Some(RuntimeStatus::ProbeFailed),
            "error" => Some(RuntimeStatus::Error),
            "stopped" => Some(RuntimeStatus::Stopped),
            _ => None,
        }
    }
}

/// An org's approval to run one pinned digest of a Plugin with its
/// PluginManifest (ADR-0072 §10).
///
/// `name` is unique per Org, **never reused** there once uninstalled, and is
/// what reports carry as `DeploymentTarget.source` — a reused name would let
/// a new installation inherit another's Deployment history and Environment
/// sources. `id` is server-minted, unique across Orgs, and names the
/// installation's ServiceAccount, so a reinstall gets a new id and the old
/// tokens map to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallation {
    pub id: String,
    pub org: String,
    pub name: String,
    /// The catalog's plugin name (the manifest's `plugin`).
    pub plugin: String,
    /// `<repository>@sha256:<digest>` (a tag only under the dev knob).
    pub image: String,
    pub manifest: PluginManifest,
    /// SHA-256 (hex) of the manifest as approved.
    pub manifest_sha256: String,
    pub config: BTreeMap<String, String>,
    pub state: InstallationState,
    pub runtime_status: RuntimeStatus,
    pub runtime_message: String,
    pub probe_passed_at: Option<i64>,
    /// The Principal subject that approved the install, and when (Unix-ms).
    pub approved_by: String,
    pub approved_at: i64,
    pub killed_by: Option<String>,
    pub killed_at: Option<i64>,
}

impl PluginInstallation {
    /// The identity this installation acts under.
    pub fn principal(&self) -> PluginPrincipal {
        PluginPrincipal::of(&self.name)
    }

    /// Check the parts an adapter or caller could get wrong: the name and id
    /// shapes, the manifest, that the manifest is the one named, and the
    /// config against it.
    pub fn validate(&self) -> Result<(), PluginError> {
        validate_installation_name(&self.name)?;
        if !is_installation_id(&self.id) {
            return Err(PluginError::Invalid(format!(
                "installation id `{}` must be {INSTALLATION_ID_HEX_CHARS} lowercase hex characters",
                self.id
            )));
        }
        self.manifest.validate()?;
        if self.manifest.plugin != self.plugin {
            return Err(PluginError::Invalid(format!(
                "installation names plugin `{}` but its manifest is `{}`",
                self.plugin, self.manifest.plugin
            )));
        }
        self.manifest.validate_config(&self.config)
    }
}

/// An installation's identity (ADR-0072 §3): `plugin:<name>`, tied to no
/// human. Under it the installation reports, calls the broker, and — once
/// named an approver (target state) — approves.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginPrincipal(String);

/// The prefix every PluginPrincipal subject carries. Reserved: no login
/// provider may mint a Principal id under it (ADR-0069 ids are
/// `<provider>:<login>`).
pub const PLUGIN_PRINCIPAL_PREFIX: &str = "plugin:";

impl PluginPrincipal {
    /// The principal of the installation named `installation_name`.
    pub fn of(installation_name: &str) -> Self {
        Self(format!("{PLUGIN_PRINCIPAL_PREFIX}{installation_name}"))
    }

    /// `plugin:<name>`, as audit and the UI show it.
    pub fn subject(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PluginPrincipal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The installation-name rule: a DNS-1123 label (it becomes a Kubernetes
/// object name in slice d2 and `DeploymentTarget.source` here).
pub fn validate_installation_name(name: &str) -> Result<(), PluginError> {
    if is_dns1123_label(name) {
        Ok(())
    } else {
        Err(PluginError::Invalid(format!(
            "installation name `{name}` must be a DNS-1123 label: 1-{MAX_INSTALLATION_NAME_CHARS} \
             lowercase letters, digits or `-`, starting and ending with a letter or digit"
        )))
    }
}

/// A server-minted installation id: exactly
/// [`INSTALLATION_ID_HEX_CHARS`] lowercase hex characters.
pub fn is_installation_id(id: &str) -> bool {
    id.len() == INSTALLATION_ID_HEX_CHARS
        && id
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// The ServiceAccount an installation's workload runs as: `plugin-<id>`.
/// Keyed by the id, not the name, so a reinstall under a new id can never
/// be reached with the old installation's tokens.
pub fn service_account_for(installation_id: &str) -> String {
    format!("plugin-{installation_id}")
}

/// The installation id a ServiceAccount name stands for, if it is one of
/// ours ([`service_account_for`]'s inverse).
pub fn installation_id_of_service_account(service_account: &str) -> Option<&str> {
    service_account
        .strip_prefix("plugin-")
        .filter(|id| is_installation_id(id))
}

fn is_dns1123_label(s: &str) -> bool {
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    !s.is_empty()
        && s.len() <= MAX_INSTALLATION_NAME_CHARS
        && s.chars().all(|c| alnum(c) || c == '-')
        && s.starts_with(alnum)
        && s.ends_with(alnum)
}

fn is_config_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// A ServiceAccount the cluster vouched for (ADR-0072 §11): a projected
/// token, audience `scarab-broker`, checked by TokenReview (slice d2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedServiceAccount {
    pub namespace: String,
    pub service_account: String,
}

/// Why a broker token was not accepted. The text is for the server's log;
/// the caller only ever sees a 401.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("plugin token rejected: {0}")]
pub struct TokenRejected(pub String);

/// Proves who is calling the broker. The production adapter is TokenReview
/// (slice d2); a verdict may be cached briefly, but the kill switch is a
/// separate check on every request, so a cache never delays revocation.
#[async_trait]
pub trait PluginTokenVerifier: Send + Sync {
    async fn verify(&self, token: &str) -> Result<VerifiedServiceAccount, TokenRejected>;
}

/// Errors from the rules in this crate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    #[error("invalid plugin manifest: {0}")]
    InvalidManifest(String),
    #[error("invalid plugin config: {0}")]
    InvalidConfig(String),
    #[error("{0}")]
    Invalid(String),
    /// The manifest asks for a capability this release cannot run (ADR-0072
    /// implementation horizon). Lists them, comma-separated.
    #[error(
        "this Scarab release cannot run plugins that ask for: {0} (only report_deployments \
         and read-only use_connection are available, ADR-0072)"
    )]
    CapabilityNotAdmitted(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(method: &str, path: &str) -> RequestRule {
        RequestRule {
            method: method.into(),
            path: path.into(),
        }
    }

    fn argo_like() -> PluginManifest {
        PluginManifest {
            plugin: "argocd".into(),
            capabilities: vec![
                PluginCapability::ReportDeployments,
                PluginCapability::UseConnection,
            ],
            connections: vec![ConnectionSlot {
                name: "argocd".into(),
                auth_schemes: vec!["bearer".into()],
                read_rules: vec![
                    rule("GET", "/api/v1/applications"),
                    rule("GET", "/api/v1/applications/*/managed-resources"),
                ],
            }],
            config_keys: vec!["ARGOCD_UI_URL".into()],
        }
    }

    #[test]
    fn the_first_party_shape_validates_and_is_admitted() {
        assert_eq!(argo_like().validate(), Ok(()));
        assert_eq!(admit_for_mmp(&argo_like()), Ok(()));
    }

    #[test]
    fn every_capability_round_trips_by_its_wire_name() {
        for capability in PluginCapability::ALL {
            let json = serde_json::to_string(&capability).unwrap();
            assert_eq!(json, format!("\"{}\"", capability.as_str()));
            assert_eq!(
                serde_json::from_str::<PluginCapability>(&json).unwrap(),
                capability
            );
        }
        assert!(serde_json::from_str::<PluginCapability>("\"run_anything\"").is_err());
    }

    #[test]
    fn admission_refuses_every_capability_beyond_the_mmp_by_name() {
        for capability in [
            PluginCapability::SubscribeEvents,
            PluginCapability::OfferActions,
            PluginCapability::ContributeUi,
            PluginCapability::ApproveGates,
        ] {
            let mut m = argo_like();
            m.capabilities.push(capability);
            assert_eq!(
                admit_for_mmp(&m),
                Err(PluginError::CapabilityNotAdmitted(
                    capability.as_str().to_string()
                )),
                "{capability}"
            );
        }
        // Reporting alone, with no connection, is admitted too.
        let report_only = PluginManifest {
            plugin: "reporter".into(),
            capabilities: vec![PluginCapability::ReportDeployments],
            connections: vec![],
            config_keys: vec![],
        };
        assert_eq!(admit_for_mmp(&report_only), Ok(()));
    }

    #[test]
    fn a_manifest_with_an_unknown_field_is_refused_not_partly_read() {
        let json = r#"{"plugin":"argocd","capabilities":["report_deployments"],
                       "write_rules":[{"method":"POST","path":"/x"}]}"#;
        assert!(serde_json::from_str::<PluginManifest>(json).is_err());
        let json = r#"{"plugin":"argocd","capabilities":["report_deployments"]}"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.validate(), Ok(()));
    }

    #[test]
    fn read_rules_are_reads_with_unambiguous_patterns() {
        for (method, path) in [
            ("POST", "/api/v1/applications"),
            ("get", "/api/v1/applications"),
            ("GET", "api/v1/applications"),
            ("GET", "/api//applications"),
            ("GET", "/api/../applications"),
            ("GET", "/api/./applications"),
            ("GET", "/api/v1/app*"),
            ("GET", "/api/v1/%2e%2e"),
            ("GET", "/api\\v1"),
            ("GET", "/api/v1?x=1"),
            ("GET", "/api/v1/ applications"),
            ("GET", "/"),
        ] {
            let mut m = argo_like();
            m.connections[0].read_rules = vec![rule(method, path)];
            assert!(
                matches!(m.validate(), Err(PluginError::InvalidManifest(_))),
                "{method} {path}"
            );
        }
        let mut m = argo_like();
        m.connections[0].read_rules = vec![rule("HEAD", "/api/v1/*/x")];
        assert_eq!(m.validate(), Ok(()));
    }

    #[test]
    fn connections_and_use_connection_come_together() {
        let mut m = argo_like();
        m.capabilities = vec![PluginCapability::ReportDeployments];
        assert!(matches!(m.validate(), Err(PluginError::InvalidManifest(_))));
        let mut m = argo_like();
        m.connections.clear();
        assert!(matches!(m.validate(), Err(PluginError::InvalidManifest(_))));
    }

    #[test]
    fn names_capabilities_slots_and_keys_are_checked() {
        type Mutation = Box<dyn Fn(&mut PluginManifest)>;
        let cases: Vec<Mutation> = vec![
            Box::new(|m| m.plugin = "Argo CD".into()),
            Box::new(|m| m.capabilities.clear()),
            Box::new(|m| m.capabilities.push(PluginCapability::ReportDeployments)),
            Box::new(|m| m.connections[0].auth_schemes = vec!["basic".into()]),
            Box::new(|m| m.connections[0].auth_schemes.clear()),
            Box::new(|m| m.connections.push(m.connections[0].clone())),
            Box::new(|m| m.config_keys.push("argocd_url".into())),
            Box::new(|m| m.config_keys.push("1URL".into())),
            Box::new(|m| m.config_keys.push("ARGOCD_UI_URL".into())),
        ];
        for (i, mutate) in cases.iter().enumerate() {
            let mut m = argo_like();
            mutate(&mut m);
            assert!(
                matches!(m.validate(), Err(PluginError::InvalidManifest(_))),
                "case {i}"
            );
        }
    }

    #[test]
    fn config_must_be_declared_and_single_line() {
        let m = argo_like();
        let ok: BTreeMap<_, _> = [("ARGOCD_UI_URL".to_string(), "https://argo".to_string())].into();
        assert_eq!(m.validate_config(&ok), Ok(()));
        let undeclared: BTreeMap<_, _> = [("TOKEN".to_string(), "x".to_string())].into();
        assert!(matches!(
            m.validate_config(&undeclared),
            Err(PluginError::InvalidConfig(_))
        ));
        let multiline: BTreeMap<_, _> = [("ARGOCD_UI_URL".to_string(), "a\nb".to_string())].into();
        assert!(matches!(
            m.validate_config(&multiline),
            Err(PluginError::InvalidConfig(_))
        ));
    }

    #[test]
    fn service_accounts_map_to_installation_ids_and_nothing_else() {
        let sa = service_account_for("0123456789");
        assert_eq!(sa, "plugin-0123456789");
        assert_eq!(installation_id_of_service_account(&sa), Some("0123456789"));
        for other in [
            "plugin-012345678",
            "plugin-0123456789a",
            "plugin-ABCDEF0123",
            "default",
            "plugin-",
            "scarab-0123456789",
        ] {
            assert_eq!(installation_id_of_service_account(other), None, "{other}");
        }
    }

    #[test]
    fn the_principal_is_plugin_colon_name() {
        assert_eq!(
            PluginPrincipal::of("argocd-prod").subject(),
            "plugin:argocd-prod"
        );
    }
}
