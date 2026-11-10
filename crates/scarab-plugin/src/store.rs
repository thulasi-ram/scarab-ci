//! The persistence port for PluginInstallations, their PluginConnections and
//! the broker journal (ADR-0072 §5, §10).

use async_trait::async_trait;

use crate::{
    BrokerJournalEntry, BrokerJournalFilter, BrokerJournalRecord, InstallationState,
    PluginConnection, PluginInstallation, PluginManifest, PluginStorage, RuntimeStatus,
};

/// A list read's page size when none is asked for, and the most one page
/// returns. Lists are keyset-paged: the next page starts after the last
/// row's key, so a row added between pages is neither skipped nor repeated.
pub const PAGE_DEFAULT_LIMIT: usize = 500;
pub const PAGE_MAX_LIMIT: usize = 1000;

/// What an uninstall removed with the installation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginUninstall {
    /// The SecretProvider keys of its PluginConnections' credentials. The
    /// rows went with the installation; the caller deletes the secrets, which
    /// live in another store (ADR-0072 §5).
    pub connection_credential_refs: Vec<String>,
}

/// What an upgrade approves (ADR-0072 §10): a new image and manifest for an
/// installation that keeps its id, name, config (less the keys the new
/// manifest no longer declares), storage, state and connections (less the
/// slots it no longer declares).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpgrade {
    pub image: String,
    pub manifest: PluginManifest,
    pub manifest_sha256: String,
    /// The source the new manifest was read from, as read.
    pub source: String,
    pub source_connection: Option<String>,
    /// The Principal subject that approved the upgrade, and when (Unix-ms).
    pub approved_by: String,
    pub approved_at: i64,
    /// The StepUpConfirmation that authorised it; `None` for a same-or-
    /// narrower upgrade, which needs none.
    pub step_up_ref: Option<String>,
}

/// What an upgrade stored, and what it unbound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpgraded {
    pub installation: PluginInstallation,
    /// The SecretProvider keys of the connections bound to slots the new
    /// manifest no longer declares: those rows went with the upgrade, and
    /// the caller deletes the secrets.
    pub connection_credential_refs: Vec<String>,
}

/// A version an installation ran before an upgrade replaced it: the history
/// an upgrade records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallationVersion {
    pub installation_id: String,
    pub image: String,
    pub manifest: PluginManifest,
    pub manifest_sha256: String,
    pub source: Option<String>,
    pub source_connection: Option<String>,
    pub approved_by: String,
    pub approved_at: i64,
    /// Who upgraded it away, when, and the step-up that authorised that, if
    /// one did.
    pub replaced_by: String,
    pub replaced_at: i64,
    pub step_up_ref: Option<String>,
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
    /// the old one's journal and anything else still naming it.
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
    /// The row changed between the caller's read and its conditional write
    /// (another admin's edit landed first). Nothing was written.
    #[error("plugin installation `{0}` changed meanwhile; reload it and try again")]
    Changed(String),
    /// The store is saturated: no connection was free in time, or a lock
    /// wait passed its timeout. Nothing was recorded; retry later.
    #[error("plugin store busy: {0}")]
    Busy(String),
    /// The backing store failed, or held a row this binary cannot read.
    #[error("plugin store: {0}")]
    Store(String),
}

/// Durable PluginInstallations, keyed by `(org, name)` and by `id`.
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

    /// Every installation in every Org, by id — what the plugin runtime
    /// reconciles workloads against. Only the runtime reads it: an id absent
    /// here is an uninstalled installation whose workload must go.
    async fn all_installations(&self) -> Result<Vec<PluginInstallation>, PluginStoreError>;

    /// Kill or re-enable. Killing records who and when; enabling clears it.
    /// Re-enabling a killed installation also resets its runtime record to
    /// `pending` with no `probe_passed_at`, so the runtime probes isolation
    /// again before it starts the workload (ADR-0072 §9: "at every start").
    /// `None` if there is no such installation.
    async fn set_installation_state(
        &self,
        org: &str,
        name: &str,
        state: InstallationState,
        by: &str,
        at: i64,
    ) -> Result<Option<PluginInstallation>, PluginStoreError>;

    /// Set (`Some`) or clear (`None`) the installation's storage claim
    /// (ADR-0074 §7) — **only if its storage is still `expected`**, what the
    /// caller read and judged the change against
    /// ([`PluginStoreError::Changed`] otherwise). `None` if there is no such
    /// installation.
    async fn set_installation_storage(
        &self,
        org: &str,
        name: &str,
        storage: Option<&PluginStorage>,
        expected: Option<&PluginStorage>,
    ) -> Result<Option<PluginInstallation>, PluginStoreError>;

    /// Upgrade the installation to `upgrade`'s image and manifest — **only if
    /// its manifest is still `expected_manifest_sha256`**, the one the caller
    /// read and judged the upgrade against ([`PluginStoreError::Changed`]
    /// otherwise; nothing is written). In one transaction: the version it
    /// replaces is recorded ([`Self::installation_versions`]); config keys
    /// and connection slots the new manifest does not declare are dropped;
    /// the state version is bumped and the runtime record reset to `pending`
    /// with no probe, so the runtime proves isolation again before the new
    /// image starts and a status decided for the old one never lands.
    /// `None` if there is no such installation.
    async fn upgrade_installation(
        &self,
        org: &str,
        name: &str,
        expected_manifest_sha256: &str,
        upgrade: &PluginUpgrade,
    ) -> Result<Option<PluginUpgraded>, PluginStoreError>;

    /// The versions upgrades replaced, oldest first.
    async fn installation_versions(
        &self,
        installation_id: &str,
    ) -> Result<Vec<PluginInstallationVersion>, PluginStoreError>;

    /// Record what the plugin runtime observed of installation `id`'s
    /// workload — written **only if the installation is still in
    /// `decided_in` at `decided_at_version`**, the state and state version
    /// the runtime read before acting. A kill or enable that lands while the
    /// runtime was probing or applying is then never overwritten by a status
    /// decided for the old state, even a kill and re-enable that leave the
    /// state as it was (git-bug da0af59); the next pass decides again.
    /// Returns whether a row was written.
    async fn set_runtime_status(
        &self,
        id: &str,
        decided_in: InstallationState,
        decided_at_version: i64,
        status: RuntimeStatus,
        message: &str,
        probe_passed_at: Option<i64>,
    ) -> Result<bool, PluginStoreError>;

    /// Uninstall: remove the installation and its PluginConnections, and
    /// retire its name in the Org for good — atomically.
    /// Returns what it removed; `None` if there was no such installation.
    async fn uninstall(
        &self,
        org: &str,
        name: &str,
        by: &str,
        at: i64,
    ) -> Result<Option<PluginUninstall>, PluginStoreError>;

    /// Bind (or re-bind) a PluginConnection, replacing the slot's previous
    /// binding whole. Returns the previous binding's `credential_ref`, whose
    /// secret the caller now deletes. [`PluginStoreError::NotFound`] if the
    /// installation is gone (an uninstall won the race); nothing is stored
    /// then. Whether the slot exists in the manifest is the caller's check.
    async fn put_connection(
        &self,
        connection: &PluginConnection,
    ) -> Result<Option<String>, PluginStoreError>;

    /// The installation's PluginConnections, by slot.
    async fn connections(
        &self,
        installation_id: &str,
    ) -> Result<Vec<PluginConnection>, PluginStoreError>;

    /// One binding — what the broker reads on every proxied request, and
    /// re-reads while a response streams, so a re-bind or unbind reaches a
    /// stream already open.
    async fn connection(
        &self,
        installation_id: &str,
        slot: &str,
    ) -> Result<Option<PluginConnection>, PluginStoreError>;

    /// Unbind a slot. Returns the removed binding (its secret is the
    /// caller's to delete); `None` if it was not bound.
    async fn delete_connection(
        &self,
        installation_id: &str,
        slot: &str,
    ) -> Result<Option<PluginConnection>, PluginStoreError>;

    /// Append broker journal entries (ADR-0072 §5), in order.
    async fn append_broker_journal(
        &self,
        entries: &[BrokerJournalEntry],
    ) -> Result<(), PluginStoreError>;

    /// The installation's journal rows `filter` selects, newest first, in
    /// one read. A row's id is the keyset: pass the last id of a page as
    /// `before` to read the next, older one.
    async fn broker_journal(
        &self,
        installation_id: &str,
        filter: &BrokerJournalFilter,
    ) -> Result<Vec<BrokerJournalRecord>, PluginStoreError>;

    /// Drop journal entries started before `started_before`. Returns how many
    /// went.
    async fn prune_broker_journal(&self, started_before: i64) -> Result<u64, PluginStoreError>;
}
