//! The log store's record types: one stored log (a **stream**), the rows of
//! its index, the writer port's refusals, and the sealed per-launch value
//! ledger of the **unit** a stream belongs to.
//!
//! A unit is what a launch creates: a step attempt `(run, step, attempt)` or
//! a shared service instance `(run, service:<name>, t<take>)`. A stream is a
//! unit's main container, or a sidecar `(run, <step>::service-<i>, attempt)`
//! of its step's unit. Both are named by a [`LogStreamId`].

use serde::{Deserialize, Serialize};

use crate::{AttemptId, DbError, RunId, StepId};

/// One stream, or one unit: `(run, step key, attempt)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogStreamId {
    pub run: RunId,
    pub step: StepId,
    pub attempt: AttemptId,
}

impl LogStreamId {
    pub fn new(run: &RunId, step: &StepId, attempt: &AttemptId) -> Self {
        Self {
            run: run.clone(),
            step: step.clone(),
            attempt: attempt.clone(),
        }
    }

    /// The unit this stream belongs to: a sidecar stream
    /// (`<step>::service-<i>`) belongs to its step's unit; every other
    /// stream is its own unit.
    pub fn unit(&self) -> LogStreamId {
        match self.step.0.rsplit_once("::service-") {
            Some((owner, _)) => LogStreamId {
                run: self.run.clone(),
                step: StepId(owner.to_string()),
                attempt: self.attempt.clone(),
            },
            None => self.clone(),
        }
    }
}

/// The resource of a stream's tail lease (ADR-0051). A replica holds it
/// while it reads the stream's source; every row it appends is fenced on
/// its term. The `tail2:` prefix is new in 0.7, so a 0.7 tail never waits on
/// a 0.6.x holder of the old `tail:` lease during a rollout. The store
/// derives the same string from the stream id
/// (`scarab_append_log_row` in migration 0099): keep the two in step.
pub fn tail_lease_resource(run: &RunId, step: &StepId, attempt: &AttemptId) -> String {
    match step.0.rsplit_once("::service-") {
        Some((owner, index)) => format!("tail2:{}:{owner}:sidecar-{index}:{}", run.0, attempt.0),
        None => format!("tail2:{}:{}:{}", run.0, step.0, attempt.0),
    }
}

/// Which lease a writer of a stream holds. The store derives the lease's
/// resource from the stream it appends to ([`LogLease::resource`]), so a
/// writer holding one stream's lease cannot append to another stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLease {
    /// The stream's tail lease ([`tail_lease_resource`]): a replica that
    /// reads the stream's source.
    Tail,
    /// The lease of a writer that feeds the store directly (tests and
    /// tools): `append:<run>:<step>:<attempt>`. A reader of the log never
    /// takes it to mean the log is still being stored.
    Append,
}

impl LogLease {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLease::Tail => "tail",
            LogLease::Append => "append",
        }
    }

    /// The lease resource of `stream` for this kind.
    pub fn resource(self, stream: &LogStreamId) -> String {
        match self {
            LogLease::Tail => tail_lease_resource(&stream.run, &stream.step, &stream.attempt),
            LogLease::Append => format!(
                "append:{}:{}:{}",
                stream.run.0, stream.step.0, stream.attempt.0
            ),
        }
    }
}

/// A writer of a stream: the kind of lease it holds and its `(owner,
/// term)`. An append is fenced on the lease of that kind for the stream it
/// appends to, whatever lease the writer took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogWriter {
    pub lease: LogLease,
    pub owner: String,
    pub term: i64,
}

impl LogWriter {
    pub fn new(lease: LogLease, owner: impl Into<String>, term: i64) -> Self {
        Self {
            lease,
            owner: owner.into(),
            term,
        }
    }

    /// The writer that holds `held`, a lease of kind `lease`.
    pub fn holding(lease: LogLease, held: &crate::LeaderTerm) -> Self {
        Self::new(lease, held.owner(), held.term())
    }

    /// The lease row an append to `stream` is fenced on.
    pub fn fence(&self, stream: &LogStreamId) -> crate::LeaderTerm {
        crate::LeaderTerm::new(self.lease.resource(stream), self.owner.clone(), self.term)
    }
}

/// What an index row is. Only `Output` rows have a body; the others are
/// markers the server writes, never derived from a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogRowKind {
    #[default]
    Output,
    /// The source restarted: the range before this row may be missing and
    /// the part after it may repeat lines.
    SourceRestarted,
    /// The rest of the log was not stored ([`NotStoredReason`]).
    NotStored,
    /// The end of the log could not be read.
    Incomplete,
}

impl LogRowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LogRowKind::Output => "output",
            LogRowKind::SourceRestarted => "source_restarted",
            LogRowKind::NotStored => "not_stored",
            LogRowKind::Incomplete => "incomplete",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "output" => LogRowKind::Output,
            "source_restarted" => LogRowKind::SourceRestarted,
            "not_stored" => LogRowKind::NotStored,
            "incomplete" => LogRowKind::Incomplete,
            _ => return None,
        })
    }

    pub fn is_marker(self) -> bool {
        self != LogRowKind::Output
    }
}

/// Why the rest of a log was not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotStoredReason {
    /// The unit has no value ledger, or its Pod was not made by a server
    /// that keeps one: an earlier Scarab version started it.
    NoLedger,
    /// The unit's value ledger was removed before the log could be read.
    LedgerPurged,
}

impl NotStoredReason {
    pub fn as_str(self) -> &'static str {
        match self {
            NotStoredReason::NoLedger => "no_ledger",
            NotStoredReason::LedgerPurged => "ledger_purged",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "no_ledger" => NotStoredReason::NoLedger,
            "ledger_purged" => NotStoredReason::LedgerPurged,
            _ => return None,
        })
    }
}

/// The text a marker row stands for: shown as its own line in the web UI,
/// sent as an SSE `marker` event, and written as `[scarab] <text>` in a raw
/// download.
pub fn marker_text(kind: LogRowKind, reason: Option<NotStoredReason>) -> &'static str {
    match (kind, reason) {
        (LogRowKind::Output, _) => "",
        (LogRowKind::SourceRestarted, _) => {
            "the log source restarted; lines before this point may be missing or repeat"
        }
        (LogRowKind::NotStored, Some(NotStoredReason::LedgerPurged)) => {
            "the rest of this log was not stored: its secret values were removed before it \
             could be read"
        }
        (LogRowKind::NotStored, _) => {
            "the rest of this log was not stored: this step was started by an earlier Scarab \
             version, so its secret values are not known"
        }
        (LogRowKind::Incomplete, _) => "the end of this log could not be read",
    }
}

/// The body of an output row: where it is stored, its length and the
/// SHA-256 of its (redacted, uncompressed) bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogBody {
    pub object_key: String,
    pub len: u64,
    pub sha256: Vec<u8>,
}

/// The state of a stream. `Complete` and `Closed` take no further row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStreamState {
    #[default]
    Open,
    /// The source's end was confirmed: every byte is stored.
    Complete,
    /// Given up, for a cause every replica would find.
    Closed,
}

impl LogStreamState {
    pub fn as_str(self) -> &'static str {
        match self {
            LogStreamState::Open => "open",
            LogStreamState::Complete => "complete",
            LogStreamState::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => LogStreamState::Open,
            "complete" => LogStreamState::Complete,
            "closed" => LogStreamState::Closed,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        self != LogStreamState::Open
    }
}

/// One row a writer appends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLogRow {
    pub kind: LogRowKind,
    pub reason: Option<NotStoredReason>,
    /// `Output` only.
    pub body: Option<LogBody>,
    /// The ledger generation the body was redacted with; required for
    /// `Output`, which commits only while it is the unit's current one.
    pub values_gen: Option<i64>,
    /// A state set in the same transaction.
    pub then: Option<LogStreamState>,
}

impl NewLogRow {
    pub fn output(body: LogBody, values_gen: i64) -> Self {
        Self {
            kind: LogRowKind::Output,
            reason: None,
            body: Some(body),
            values_gen: Some(values_gen),
            then: None,
        }
    }

    pub fn marker(kind: LogRowKind, reason: Option<NotStoredReason>) -> Self {
        Self {
            kind,
            reason,
            body: None,
            values_gen: None,
            then: None,
        }
    }

    pub fn then(mut self, state: LogStreamState) -> Self {
        self.then = Some(state);
        self
    }
}

/// Why a fenced append (or state change) was refused. Nothing was written.
#[derive(Debug, thiserror::Error)]
pub enum LogAppendError {
    /// The caller's tail lease term no longer holds.
    #[error("not the stream's writer: its tail lease moved")]
    NotWriter,
    /// The stream is `complete` or `closed`.
    #[error("the stream takes no further row")]
    Closed,
    /// The unit's ledger moved to generation `gen` since the batch was
    /// redacted.
    #[error("the unit's secret values changed (generation {gen})")]
    ValuesChanged { gen: i64 },
    /// An output row for a unit with no ledger.
    #[error("the unit has no value ledger")]
    NoLedger,
    /// An output row for a unit whose ledger was purged.
    #[error("the unit's value ledger was purged")]
    LedgerPurged,
    #[error(transparent)]
    Db(#[from] DbError),
}

/// A stream's `log_streams` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogStreamRow {
    pub next_seq: u64,
    pub byte_len: u64,
    /// The first seq of the current part (after the newest
    /// `source_restarted` row).
    pub part_start_seq: u64,
    pub state: LogStreamState,
    pub pruned: bool,
    /// When a final read of the stream first could not proceed: the start
    /// of the durable final-read budget.
    pub final_since: Option<i64>,
}

/// A unit's `log_units` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogUnitRow {
    pub gen: i64,
    pub handle: Option<String>,
    pub started_at: Option<i64>,
    pub settled_at: Option<i64>,
    pub revoke_attempted_at: Option<i64>,
    pub purged: bool,
    /// When the row was created: of two units with one handle (a Pod name
    /// hash collision), the newer is the one a handle names.
    pub created_at: i64,
    /// How many streams the unit has: its main container and one per
    /// sidecar service of its step. Its ledger is purged after its last
    /// stream ends only once this many stream rows are terminal.
    pub stream_count: i32,
    /// Failed revocations by the ledger sweep, and when it tries again.
    pub revoke_failures: i32,
    pub revoke_retry_at: Option<i64>,
}

/// One sealed generation of a unit's ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedGeneration {
    pub gen: i64,
    pub sealed: Vec<u8>,
    pub created_at: i64,
    /// When every minted forge token in it was revoked: it is not revoked
    /// again.
    pub revoked_at: Option<i64>,
}

/// A unit's ledger as stored: its row and every generation present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLedgerRows {
    pub unit: LogUnitRow,
    pub generations: Vec<SealedGeneration>,
}

/// A unit the purge sweep may purge, and whether its oldest generation is
/// past the hard cap (then it is purged even if its revocation failed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeCandidate {
    pub unit: LogStreamId,
    pub row: LogUnitRow,
    pub past_cap: bool,
}

/// A unit that left `Running` with a stream that is not terminal, found by
/// the pending-final-read query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFinalRead {
    pub unit: LogStreamId,
    /// The executor handle recorded for it: the unit's `log_units.handle`,
    /// else the attempt's or service's recorded handle.
    pub handle: Option<String>,
    /// `false`: the unit has no `log_units` row (an earlier version launched
    /// it, or it never launched).
    pub has_ledger: bool,
    /// The unit has rows stored, or was seen running: a stream of a unit
    /// with no ledger gets `not_stored` then, else nothing.
    pub started: bool,
    /// The unit's `stream_count` (1 for a unit with no ledger: its sidecars
    /// are read from its step's spec).
    pub stream_count: i32,
}

/// Where each leg of the pending-final-read query resumes: after the last
/// key a pass read, or from the start (`None`). A leg that returned fewer
/// rows than its limit starts again from the start (it wraps), so a window
/// of units that keep failing cannot hide the units behind it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingReadCursor {
    /// Units with a ledger (`log_units`), by `(run, step, attempt)`.
    pub ledger: Option<LogStreamId>,
    /// Units an earlier version launched (`log_legacy_units`).
    pub legacy: Option<LogStreamId>,
    /// Step attempts claimed since the log store's epoch with no ledger, by
    /// `(started_at, run, step, attempt)`.
    pub recent: Option<(i64, LogStreamId)>,
}

/// One page of the pending-final-read query and the cursor for the next.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingReads {
    pub reads: Vec<PendingFinalRead>,
    pub next: PendingReadCursor,
}

/// How far before the log store's epoch (the migration that added the
/// ledger) a step attempt with no ledger is still read once more: the
/// server's clock (`attempts.started_at`) and the database's (the epoch)
/// may differ by this much. An attempt this wrongly includes is closed
/// with `not_stored`, which stores nothing.
pub const PENDING_EPOCH_SKEW_MS: i64 = 120_000;

/// How long after its claim a step attempt with no ledger is still read
/// once more: the longest step timeout and a day.
pub const PENDING_RECENT_WINDOW_MS: i64 =
    (scarab_pipeline::MAX_STEP_TIMEOUT_SECS as i64 + 24 * 3600) * 1000;

/// What kind of value a launch mounts into a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountedValueKind {
    ForgeToken,
    OidcToken,
    Secret,
    RegistryAuth,
    RegistryCredential,
}

/// A repository a forge token is for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MountedRepo {
    pub owner: String,
    pub name: String,
}

/// One value a launch mounts into a unit, as a ledger generation records it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MountedValue {
    pub kind: MountedValueKind,
    /// The value's bytes, base64 in JSON.
    #[serde(with = "b64")]
    pub value: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<MountedRepo>,
    /// A forge token the forge minted for this launch (revocable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minted: Option<bool>,
}

mod b64 {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)
    }
}

impl crate::StepSpec {
    /// Every value this launch spec mounts where the unit's containers can
    /// read it: the forge token, the OIDC token, each resolved secret, the
    /// registry dockerconfigjson with each credential in it, and the
    /// forge-derived registry credential. Empty values are left out. The
    /// workspace and results tokens are not here: the fetcher and the egress
    /// sidecar hold them, and their output is not stored.
    pub fn mounted_secret_values(&self) -> Vec<MountedValue> {
        let mut out = Vec::new();
        let mut push = |kind, value: Vec<u8>, repo: Option<MountedRepo>, minted| {
            if !value.is_empty() {
                out.push(MountedValue {
                    kind,
                    value,
                    repo,
                    minted,
                });
            }
        };
        if let Some(config) = &self.forge_token {
            if let Some(c) = &config.credential {
                push(
                    MountedValueKind::ForgeToken,
                    c.token.clone().into_bytes(),
                    Some(MountedRepo {
                        owner: config.owner.clone(),
                        name: config.name.clone(),
                    }),
                    Some(c.minted),
                );
            }
        }
        if let Some(token) = &self.oidc_token {
            push(
                MountedValueKind::OidcToken,
                token.clone().into_bytes(),
                None,
                None,
            );
        }
        for (_, value) in self.resolved_secrets.iter() {
            push(MountedValueKind::Secret, value.clone(), None, None);
        }
        if let Some(r) = &self.registry_auth {
            for v in
                registry_secret_values(r.registry_auth_json.as_deref(), r.derived_auth.as_ref())
            {
                push(MountedValueKind::RegistryAuth, v.into_bytes(), None, None);
            }
            if let Some(cred) = &r.derived_auth {
                // The dockerconfigjson the executor builds from it.
                use base64::Engine;
                let auth = base64::engine::general_purpose::STANDARD
                    .encode(format!("{}:{}", cred.username, cred.token));
                let json =
                    serde_json::json!({ "auths": { cred.registry.clone(): { "auth": auth } } })
                        .to_string();
                push(
                    MountedValueKind::RegistryCredential,
                    json.into_bytes(),
                    None,
                    None,
                );
            }
        }
        out
    }
}

/// The values in a registry credential that a log must never show: the
/// dockerconfigjson itself, each `auth`, `password`, `identitytoken` and
/// `registrytoken` in it, and the password half of each decoded `auth`; or
/// the forge-derived token and the `auth` Scarab builds from it.
pub fn registry_secret_values(
    json: Option<&str>,
    derived: Option<&crate::RegistryCredential>,
) -> Vec<String> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = Vec::new();
    if let Some(json) = json {
        out.push(json.to_string());
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
        if let Some(auths) = parsed.get("auths").and_then(|a| a.as_object()) {
            for entry in auths.values() {
                for key in ["auth", "password", "identitytoken", "registrytoken"] {
                    if let Some(v) = entry.get(key).and_then(|v| v.as_str()) {
                        out.push(v.to_string());
                    }
                }
                let decoded = entry
                    .get("auth")
                    .and_then(|v| v.as_str())
                    .and_then(|a| b64.decode(a).ok())
                    .and_then(|d| String::from_utf8(d).ok());
                if let Some((_, password)) = decoded.as_deref().and_then(|d| d.split_once(':')) {
                    out.push(password.to_string());
                }
            }
        }
    }
    if let Some(cred) = derived {
        out.push(cred.token.clone());
        out.push(b64.encode(format!("{}:{}", cred.username, cred.token)));
    }
    out.retain(|v| !v.is_empty());
    out
}

/// How long a unit's ledger may be kept, measured from its oldest
/// generation, once the unit is not running: the longest step timeout, the
/// provisioning deadline and an hour.
pub const LEDGER_HARD_CAP_MS: i64 = (scarab_pipeline::MAX_STEP_TIMEOUT_SECS as i64
    + crate::scheduler::PROVISIONING_DEADLINE_SECS as i64
    + 3600)
    * 1000;

/// How long after a unit settles (or its run ends) its ledger is kept at
/// most, whatever its streams.
pub const LEDGER_SETTLED_KEEP_MS: i64 = 3600 * 1000;

/// The ledger sweep's wait after a unit's first failed revocation; it
/// doubles with each failure up to [`LEDGER_REVOKE_BACKOFF_MAX_MS`].
pub const LEDGER_REVOKE_BACKOFF_MS: i64 = 60 * 1000;
pub const LEDGER_REVOKE_BACKOFF_MAX_MS: i64 = 3600 * 1000;

/// The wait after `failures` failed revocations of a unit.
pub fn ledger_revoke_backoff_ms(failures: i32) -> i64 {
    let doublings = failures.saturating_sub(1).clamp(0, 16) as u32;
    LEDGER_REVOKE_BACKOFF_MS
        .saturating_mul(1 << doublings)
        .min(LEDGER_REVOKE_BACKOFF_MAX_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sidecar_stream_belongs_to_its_steps_unit() {
        let s = LogStreamId::new(
            &RunId("r".into()),
            &StepId("build::service-1".into()),
            &AttemptId("a1".into()),
        );
        assert_eq!(s.unit().step, StepId("build".into()));
        let svc = LogStreamId::new(
            &RunId("r".into()),
            &StepId("service:db".into()),
            &AttemptId("t1".into()),
        );
        assert_eq!(svc.unit(), svc);
    }

    #[test]
    fn a_writer_is_fenced_on_the_lease_of_the_stream_it_appends_to() {
        let main = LogStreamId::new(
            &RunId("r".into()),
            &StepId("build".into()),
            &AttemptId("a1".into()),
        );
        let side = LogStreamId::new(
            &RunId("r".into()),
            &StepId("build::service-1".into()),
            &AttemptId("a1".into()),
        );
        let w = LogWriter::new(LogLease::Tail, "o", 3);
        assert_eq!(w.fence(&main).resource(), "tail2:r:build:a1");
        assert_eq!(w.fence(&side).resource(), "tail2:r:build:sidecar-1:a1");
        assert_eq!(
            LogLease::Append.resource(&main),
            "append:r:build:a1".to_string()
        );
    }

    #[test]
    fn a_ledger_generation_round_trips_bytes() {
        let v = MountedValue {
            kind: MountedValueKind::Secret,
            value: vec![0, 255, b'a'],
            repo: None,
            minted: None,
        };
        let json = serde_json::to_string(&vec![v.clone()]).unwrap();
        let back: Vec<MountedValue> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, vec![v]);
    }
}
