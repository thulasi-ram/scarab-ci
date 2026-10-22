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

/// The most DeploymentTargets one installation may report on, and the most
/// groupings no Environment lists it may have counted. Each is a row core
/// keeps for good, created by whatever a plugin sends; without a cap a buggy
/// or hostile plugin grows them without bound (red-team F-2). Far above any
/// real Argo CD (thousands of Applications), and a cap blocks only NEW rows:
/// a target already known keeps reporting. An item over a cap is
/// `rejected` with a `cap:` reason.
pub const MAX_TARGETS_PER_INSTALLATION: u64 = 10_000;
pub const MAX_UNMAPPED_GROUPINGS_PER_INSTALLATION: u64 = 1_000;

/// A list read's page size when none is asked for, and the most one page
/// returns. Lists are keyset-paged: the next page starts after the last
/// row's key, so a row added between pages is neither skipped nor repeated.
pub const PAGE_DEFAULT_LIMIT: usize = 500;
pub const PAGE_MAX_LIMIT: usize = 1000;

/// An EnvironmentDeploymentSource that names an installation, seen from the
/// installation: the Environment it routes a grouping into.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct InstallationDeploymentSource {
    pub environment: String,
    pub vendor_grouping: String,
}

/// What core knows of one DeploymentTarget a source reports on: its cursor
/// (the last accepted sequence), where it routes now, where its last status
/// was recorded, its declared version scheme, its last reported status and
/// its owners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentSourceTarget {
    pub installation_id: String,
    /// The installation's name — `DeploymentTarget.source`.
    pub installation: String,
    pub target_id: String,
    pub vendor_grouping: String,
    /// Where a report about it routes **now**: the Environment that lists
    /// *(installation, grouping)* at the moment of the read. `None` while
    /// none does. Moves the instant an admin edits the sources — no report
    /// needed.
    pub environment: Option<String>,
    /// The Environment its last status report was **appended to**; `None`
    /// if its last status was dropped (unmapped) or none was ever reported.
    pub status_environment: Option<String>,
    /// The reporter must re-send this target's current status: it routes
    /// somewhere ([`Self::environment`]) that its last status never reached
    /// ([`Self::status_environment`]) — a grouping an admin just mapped, or
    /// moved to another Environment. Derived on read, never stored, so an
    /// Environment edit needs no write here and only a status report that
    /// lands clears it (ADR-0070 §11).
    pub resend: bool,
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

/// One page of an installation's targets ([`PluginStore::source_targets`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceTargetsPage {
    /// Start after this target id (exclusive); `None` = from the first.
    pub after: Option<String>,
    /// At most this many; clamped to `1..=`[`PAGE_MAX_LIMIT`].
    pub limit: usize,
    /// Only the targets whose [`DeploymentSourceTarget::resend`] is set —
    /// what a reporter polls for.
    pub resend_only: bool,
}

/// One page of an Environment's targets
/// ([`PluginStore::environment_targets`]), ordered by installation name then
/// target id.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvironmentTargetsPage {
    /// Start after this `(installation name, target id)` (exclusive).
    pub after: Option<(String, String)>,
    /// At most this many; clamped to `1..=`[`PAGE_MAX_LIMIT`].
    pub limit: usize,
}

/// Where one DeploymentTarget a source has reported on routes now
/// ([`PluginStore::target_routings`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRouting {
    /// The installation's name — `DeploymentTarget.source`.
    pub installation: String,
    pub target_id: String,
    /// `None` = no Environment lists its grouping any more.
    pub environment: Option<String>,
}

/// How much of its caps an installation uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InstallationUsage {
    /// Out of [`MAX_TARGETS_PER_INSTALLATION`].
    pub targets: u64,
    /// Rows counted against [`MAX_UNMAPPED_GROUPINGS_PER_INSTALLATION`] —
    /// including groupings an Environment has listed since, until their next
    /// report clears them.
    pub unmapped_groupings: u64,
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
    /// The store is saturated: no connection was free in time, or a lock
    /// wait passed its timeout. Nothing was recorded; retry later.
    #[error("plugin store busy: {0}")]
    Busy(String),
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
    /// still advanced); a NEW target or unmapped grouping past its cap
    /// ([`MAX_TARGETS_PER_INSTALLATION`],
    /// [`MAX_UNMAPPED_GROUPINGS_PER_INSTALLATION`]) is rejected; otherwise a
    /// status is appended to the Deployment record. Refuses a killed
    /// installation, an idempotency key reused with other content, and a
    /// saturated store ([`PluginStoreError::Busy`]) — then nothing in the
    /// batch is stored.
    async fn ingest_report(
        &self,
        installation_id: &str,
        items: &[ReportIngest],
        received_at: i64,
    ) -> Result<Vec<ReportItemOutcome>, PluginStoreError>;

    /// A page of the installation's targets — what a restarted plugin seeds
    /// its cursors from, and (with `resend_only`) what it must re-send — by
    /// target id.
    async fn source_targets(
        &self,
        installation_id: &str,
        page: &SourceTargetsPage,
    ) -> Result<Vec<DeploymentSourceTarget>, PluginStoreError>;

    /// A page of the reported targets whose grouping `environment` lists
    /// **now**, in `org`, by installation then target id. Their `last_*`
    /// fields are set only when that last status was recorded in
    /// `environment`: a status reported elsewhere is not this Environment's
    /// fact.
    async fn environment_targets(
        &self,
        org: &str,
        environment: &str,
        page: &EnvironmentTargetsPage,
    ) -> Result<Vec<DeploymentSourceTarget>, PluginStoreError>;

    /// Where each of `targets` — `(installation name, target id)` pairs in
    /// `org` — routes now. A pair no installation of the Org has reported on
    /// is absent.
    async fn target_routings(
        &self,
        org: &str,
        targets: &[(String, String)],
    ) -> Result<Vec<TargetRouting>, PluginStoreError>;

    /// A page of the installation's groupings no Environment lists right
    /// now, by grouping, starting after `after`.
    async fn unmapped_groupings(
        &self,
        installation_id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<UnmappedGrouping>, PluginStoreError>;

    /// How much of its caps the installation uses.
    async fn installation_usage(
        &self,
        installation_id: &str,
    ) -> Result<InstallationUsage, PluginStoreError>;

    /// Drop report receipts received before `received_before`. Returns how
    /// many went.
    async fn prune_report_receipts(&self, received_before: i64) -> Result<u64, PluginStoreError>;
}
