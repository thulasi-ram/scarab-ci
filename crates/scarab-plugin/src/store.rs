//! The persistence port for PluginInstallations, their PluginConnections and
//! the broker journal (ADR-0072 §5, §10).

use async_trait::async_trait;

use crate::{
    BrokerJournalEntry, InstallationState, PluginConnection, PluginInstallation, RuntimeStatus,
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

    /// Record what the plugin runtime observed of installation `id`'s
    /// workload — written **only if the installation is still in
    /// `decided_in`**, the state the runtime read before acting. A kill or
    /// enable that lands while the runtime was probing or applying is then
    /// never overwritten by a status decided for the old state; the next pass
    /// decides again. Returns whether a row was written.
    async fn set_runtime_status(
        &self,
        id: &str,
        decided_in: InstallationState,
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

    /// The installation's newest journal entries, newest first.
    async fn broker_journal(
        &self,
        installation_id: &str,
        limit: usize,
    ) -> Result<Vec<BrokerJournalEntry>, PluginStoreError>;

    /// Drop journal entries started before `started_before`. Returns how many
    /// went.
    async fn prune_broker_journal(&self, started_before: i64) -> Result<u64, PluginStoreError>;
}
