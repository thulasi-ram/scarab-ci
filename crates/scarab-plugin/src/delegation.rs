//! DelegatedCallerHandles and their journal (ADR-0086 §3, §4): the port
//! PluginApi stores them through.
//!
//! A Plugin never holds a caller's credential. With each request core
//! forwards to it, core gives it a handle: an opaque value with which it
//! calls core as that caller, through PluginApi on the broker listener. The
//! handle's authority is the intersection of the caller's PrincipalToken,
//! the endpoint operation's `may`, and its own counters. It is a row, so any
//! server replica can honour it.

use async_trait::async_trait;

use crate::PluginStoreError;

/// The longest a handle lives, whatever its connection does (ADR-0086 §3).
pub const DELEGATED_CALLER_HANDLE_MAX_LIFETIME_MS: i64 = 10 * 60 * 1000;

/// A handle whose holder has not heartbeated it for this long is refused:
/// the replica holding the inbound connection heartbeats every minute while
/// the connection is open (ADR-0086 §3).
pub const DELEGATED_CALLER_HANDLE_HEARTBEAT_STALE_MS: i64 = 2 * 60 * 1000;

/// How many core calls one handle may make (ADR-0086 §3).
pub const DELEGATED_CALLER_HANDLE_MAX_CALLS: u32 = 50;

/// How long a delegated call's journal row is kept: as long as the broker
/// journal's ([`crate::BROKER_JOURNAL_RETENTION_MS`]).
pub const DELEGATED_CALL_JOURNAL_RETENTION_MS: i64 = crate::BROKER_JOURNAL_RETENTION_MS;

/// One DelegatedCallerHandle as stored. The value the Plugin presents is not
/// here: the store keys the row by its SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedCallerHandle {
    /// Row id. Safe to log; not the credential.
    pub id: String,
    /// The one installation that may present it.
    pub installation_id: String,
    /// That installation's org.
    pub org: String,
    /// The caller's PrincipalToken, by row id. Re-read on every call, so a
    /// revoked or expired token stops its handles at once.
    pub token_id: String,
    /// The ApiOperation ids it may use: the endpoint operations' `may`,
    /// narrowed to the token's own operation list when it has one.
    pub operations: Vec<String>,
    /// Writes it may still make.
    pub writes_left: u32,
    /// Core calls it may still make.
    pub calls_left: u32,
    /// Unix-ms.
    pub created_at: i64,
    /// Unix-ms of the last heartbeat; the mint counts as one.
    pub heartbeat_at: i64,
    /// Unix-ms; at most [`DELEGATED_CALLER_HANDLE_MAX_LIFETIME_MS`] after
    /// `created_at`.
    pub expires_at: i64,
}

impl DelegatedCallerHandle {
    /// May it be presented at `now_ms`: not past its expiry, and heartbeated
    /// within [`DELEGATED_CALLER_HANDLE_HEARTBEAT_STALE_MS`]?
    pub fn is_live(&self, now_ms: i64) -> bool {
        now_ms < self.expires_at
            && now_ms - self.heartbeat_at < DELEGATED_CALLER_HANDLE_HEARTBEAT_STALE_MS
    }

    /// Does it allow the ApiOperation `operation`?
    pub fn allows(&self, operation: &str) -> bool {
        self.operations.iter().any(|o| o == operation)
    }
}

/// What spending one call of a handle did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegatedCallSpend {
    /// One call (and one write, when asked) was taken.
    Spent,
    /// It has no call left, or no write left for a write: nothing taken.
    Exhausted,
    /// It no longer exists or has expired: nothing taken.
    Gone,
}

/// One delegated call as the journal records it before the call runs. A
/// call that cannot be journaled is not made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedCallJournalEntry {
    pub installation_id: String,
    pub org: String,
    pub handle_id: String,
    /// The Principal the call acts as.
    pub caller_subject: String,
    /// The PrincipalToken behind the handle, and its kind and OAuth client.
    pub token_id: String,
    pub token_kind: String,
    pub oauth_client: Option<String>,
    /// The ApiOperation of the route it called.
    pub operation: String,
    pub method: String,
    /// The path called, without its query.
    pub path: String,
    pub request_id: String,
    /// Unix-ms.
    pub started_at: i64,
}

/// Where PluginApi keeps its handles and journal.
#[async_trait]
pub trait DelegatedCallStore: Send + Sync {
    /// Store a freshly minted handle; `hash` is the SHA-256 (hex) of the
    /// value handed to the Plugin.
    async fn put_delegated_caller_handle(
        &self,
        handle: &DelegatedCallerHandle,
        hash: &str,
    ) -> Result<(), PluginStoreError>;

    /// The handle a presented value hashes to, live or not.
    async fn delegated_caller_handle_by_hash(
        &self,
        hash: &str,
    ) -> Result<Option<DelegatedCallerHandle>, PluginStoreError>;

    /// Take one call, and one write when `write`, from handle `id`, in one
    /// conditional write that refuses at zero and past expiry.
    async fn spend_delegated_caller_handle(
        &self,
        id: &str,
        write: bool,
        now_ms: i64,
    ) -> Result<DelegatedCallSpend, PluginStoreError>;

    /// Record that the connection the handle serves is still open. `false`
    /// when the handle is gone or expired.
    async fn heartbeat_delegated_caller_handle(
        &self,
        id: &str,
        now_ms: i64,
    ) -> Result<bool, PluginStoreError>;

    /// Delete handle `id` (its connection closed). Deleting an unknown id
    /// is a no-op.
    async fn delete_delegated_caller_handle(&self, id: &str) -> Result<(), PluginStoreError>;

    /// Delete every handle of `installation_id` (it was uninstalled or
    /// killed); returns how many.
    async fn delete_installation_delegated_caller_handles(
        &self,
        installation_id: &str,
    ) -> Result<u64, PluginStoreError>;

    /// Delete the handles expired at `now_ms`; returns how many.
    async fn delete_expired_delegated_caller_handles(
        &self,
        now_ms: i64,
    ) -> Result<u64, PluginStoreError>;

    /// Journal a call before it runs; returns the row's id.
    async fn journal_delegated_call(
        &self,
        entry: &DelegatedCallJournalEntry,
    ) -> Result<i64, PluginStoreError>;

    /// Record the status a journaled call answered with.
    async fn finish_delegated_call(
        &self,
        journal_id: i64,
        status: u16,
        now_ms: i64,
    ) -> Result<(), PluginStoreError>;

    /// Delete journal rows started before `started_before`; returns how
    /// many.
    async fn prune_delegated_call_journal(
        &self,
        started_before: i64,
    ) -> Result<u64, PluginStoreError>;
}
