//! The persistence port for PluginInstallations and the report ingest
//! (ADR-0072 §10-11).
//!
//! Idempotency and source sequences live in the store's **side tables**,
//! never in the Deployment record: the record's input log keeps only facts
//! the fold reads, so it stays vendor-neutral. Ownership is a current
//! snapshot per *(installation, target)*, likewise outside the fold.

use async_trait::async_trait;

use crate::{InstallationState, PluginInstallation, ReportIngest, ReportItemOutcome};

/// How long a report receipt (idempotency key → outcome) is kept. A repeat
/// older than this is no longer recognised as a duplicate, but its sequence
/// is still not newer than the target's cursor, so it is dropped as stale —
/// the receipt only makes the answer exact for a prompt retry.
pub const REPORT_RECEIPT_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// An EnvironmentDeploymentSource that names an installation, seen from the
/// installation: the Environment it routes a grouping into.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct InstallationDeploymentSource {
    pub environment: String,
    pub vendor_grouping: String,
}

/// What core knows of one DeploymentTarget a source reports on: its cursor
/// (the last accepted sequence), where it routes, its declared version
/// scheme, its last reported status and its owners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentSourceTarget {
    pub installation_id: String,
    /// The installation's name — `DeploymentTarget.source`.
    pub installation: String,
    pub target_id: String,
    pub vendor_grouping: String,
    /// `None` while no Environment lists the grouping.
    pub environment: Option<String>,
    pub last_sequence: u64,
    /// `None` until a status report has declared one.
    pub version_scheme: Option<String>,
    pub last_version: Option<String>,
    /// `in_progress` / `live` / `failed`, as last reported.
    pub last_status: Option<String>,
    pub last_label: Option<String>,
    /// `org/repo`, sorted.
    pub owner_projects: Vec<String>,
    pub owners_reported_at: Option<i64>,
    pub updated_at: i64,
}

/// A vendor grouping an installation reports on that no Environment lists:
/// what an admin needs to decide where it belongs (ADR-0071 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmappedGrouping {
    pub vendor_grouping: String,
    /// Reports dropped for it so far.
    pub reports: u64,
    pub first_at: i64,
    pub last_at: i64,
    pub last_target_id: String,
}

/// A [`PluginStore`] failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginStoreError {
    #[error("{0}")]
    Invalid(String),
    /// An installation by that name exists in the Org.
    #[error("plugin installation `{0}` already exists")]
    NameTaken(String),
    /// The name belonged to an installation that was uninstalled. Names are
    /// never reused within an Org: a new installation would otherwise inherit
    /// the old one's Deployment history and any Environment source still
    /// naming it.
    #[error(
        "plugin installation name `{0}` was used by an uninstalled installation and is \
         retired; choose another name"
    )]
    NameRetired(String),
    /// The minted id collided with another installation's; mint another.
    #[error("installation id collision")]
    IdTaken,
    /// No such installation (it was uninstalled, or never existed).
    #[error("no such plugin installation")]
    NotFound,
    /// The installation is killed; nothing it sends is accepted.
    #[error("plugin installation is killed")]
    Killed,
    /// An idempotency key was reused with different content. The whole batch
    /// was rolled back.
    #[error(
        "idempotency key `{idempotency_key}` was already used for a different report; \
         nothing in this batch was recorded"
    )]
    IdempotencyConflict { idempotency_key: String },
    /// The backing store failed, or held a row this binary cannot read.
    #[error("plugin store: {0}")]
    Store(String),
}

/// Durable PluginInstallations, keyed by `(org, name)` and by `id`, and the
/// report ingest that writes into the Deployment record.
#[async_trait]
pub trait PluginStore: Send + Sync {
    /// Record a new installation. Refuses a name already in use
    /// ([`PluginStoreError::NameTaken`]), one used before
    /// ([`PluginStoreError::NameRetired`]) and a colliding id
    /// ([`PluginStoreError::IdTaken`]); nothing is stored then.
    async fn install(
        &self,
        installation: &PluginInstallation,
    ) -> Result<PluginInstallation, PluginStoreError>;

    async fn installation(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Option<PluginInstallation>, PluginStoreError>;

    /// By server-minted id — what a verified ServiceAccount maps to. Read on
    /// every broker request: this is the kill switch's check.
    async fn installation_by_id(
        &self,
        id: &str,
    ) -> Result<Option<PluginInstallation>, PluginStoreError>;

    /// Every installation in `org`, by name.
    async fn installations(&self, org: &str) -> Result<Vec<PluginInstallation>, PluginStoreError>;

    /// Kill or re-enable. Killing records who and when; enabling clears it.
    /// `None` if there is no such installation.
    async fn set_installation_state(
        &self,
        org: &str,
        name: &str,
        state: InstallationState,
        by: &str,
        at: i64,
    ) -> Result<Option<PluginInstallation>, PluginStoreError>;

    /// Uninstall: remove the installation, its cursors, receipts and unmapped
    /// counts, **and every EnvironmentDeploymentSource naming it**, and retire
    /// its name in the Org for good — atomically. Returns the sources it
    /// removed; `None` if there was no such installation.
    async fn uninstall(
        &self,
        org: &str,
        name: &str,
        by: &str,
        at: i64,
    ) -> Result<Option<Vec<InstallationDeploymentSource>>, PluginStoreError>;

    /// The EnvironmentDeploymentSources in `org` that name the installation
    /// `name` (whether or not it exists — a source written before
    /// installations were checked may name one in advance), sorted.
    async fn deployment_sources_naming(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Vec<InstallationDeploymentSource>, PluginStoreError>;

    /// Ingest validated report items from installation `installation_id`, in
    /// order, **in one transaction**, at `received_at` (the control plane's
    /// clock, read right before the call). Org and source come from the
    /// installation's own record, never from a report. Per item: a repeat
    /// key is a duplicate; a sequence not newer than the target's cursor is
    /// stale; a grouping no Environment lists is unmapped (counted, sequence
    /// still advanced); otherwise a status is appended to the Deployment
    /// record. Refuses a killed installation and an idempotency key reused
    /// with other content — then nothing in the batch is stored.
    async fn ingest_report(
        &self,
        installation_id: &str,
        items: &[ReportIngest],
        received_at: i64,
    ) -> Result<Vec<ReportItemOutcome>, PluginStoreError>;

    /// The installation's targets — what a restarted plugin seeds its
    /// cursors from — by target id.
    async fn source_targets(
        &self,
        installation_id: &str,
    ) -> Result<Vec<DeploymentSourceTarget>, PluginStoreError>;

    /// Every reported target routed into `environment` in `org`, by
    /// installation then target id.
    async fn environment_targets(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<DeploymentSourceTarget>, PluginStoreError>;

    /// The installation's groupings no Environment lists right now, by
    /// grouping.
    async fn unmapped_groupings(
        &self,
        installation_id: &str,
    ) -> Result<Vec<UnmappedGrouping>, PluginStoreError>;

    /// Drop report receipts received before `received_before`. Returns how
    /// many went.
    async fn prune_report_receipts(&self, received_before: i64) -> Result<u64, PluginStoreError>;
}
