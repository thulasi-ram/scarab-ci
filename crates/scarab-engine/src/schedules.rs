//! The cron schedule index (git-bug 5d5fc8a).
//!
//! One row per `(org, project, pipeline, schedule)`: an `on: cron:`
//! expression a Pipeline file on the default branch declares, and when it
//! next fires. The index decides only **when** the server checks; at each
//! fire the server reads the default branch, its head and the Pipeline from
//! the forge again, and fires only if that Pipeline still declares a schedule
//! for the minute. So a stale row can cost a check or a missed fire, never a
//! Run of old config.
//!
//! Each fired slot is a row keyed by `(org, project, pipeline, slot_at)`.
//! Claiming a slot inserts it; a second claim of the same slot, from another
//! replica or from a second schedule of the same Pipeline that names the
//! same minute, finds it and starts nothing.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{DbError, Timestamp};

/// One indexed schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleEntry {
    pub org: String,
    pub project: String,
    /// The Pipeline file, `.scarab/<name>.yaml`.
    pub pipeline: String,
    /// The cron expression as written (trimmed).
    pub schedule: String,
    /// The next slot, a whole minute in Unix milliseconds.
    pub next_fire_at: Timestamp,
    /// The default-branch commit the row was read from.
    pub source_sha: String,
}

/// One claimed slot and what came of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleFire {
    pub org: String,
    pub project: String,
    pub pipeline: String,
    /// The schedule that claimed the slot.
    pub schedule: String,
    /// The slot, a whole minute in Unix milliseconds.
    pub slot_at: Timestamp,
    pub fired_at: Timestamp,
    /// `pending` while the Run is being made, then `run` (a Run was made),
    /// `refused` (a refused Run was recorded), `skipped` (the Pipeline's
    /// `when:` or step guards left nothing to run) or `missed` (the forge
    /// could not be read in time).
    pub outcome: String,
    pub run_id: Option<String>,
}

/// The schedule index and the fired slots.
#[async_trait]
pub trait ScheduleStore: Send + Sync {
    /// Make `entries` the project's whole index, all read from `source_sha`.
    /// A row whose `(pipeline, schedule)` is in both keeps its
    /// `next_fire_at`; a new one takes the entry's; a row not in `entries` is
    /// deleted. One transaction.
    async fn replace_project_schedules(
        &self,
        org: &str,
        project: &str,
        source_sha: &str,
        entries: &[ScheduleEntry],
    ) -> Result<(), DbError>;

    /// The project's rows, ordered by pipeline then schedule.
    async fn project_schedules(
        &self,
        org: &str,
        project: &str,
    ) -> Result<Vec<ScheduleEntry>, DbError>;

    /// Rows due at or before `before`, soonest first, at most `limit`.
    async fn due_schedules(
        &self,
        before: Timestamp,
        limit: u32,
    ) -> Result<Vec<ScheduleEntry>, DbError>;

    /// Move one row's next fire from `from` to `to`. `false` when the row is
    /// gone or no longer at `from` (another writer moved it).
    async fn advance_schedule(
        &self,
        entry: &ScheduleEntry,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<bool, DbError>;

    /// Claim a slot by inserting `fire`. `false` when the slot was already
    /// claimed.
    async fn claim_fire(&self, fire: &ScheduleFire) -> Result<bool, DbError>;

    /// Record what came of a claimed slot.
    async fn finish_fire(
        &self,
        org: &str,
        project: &str,
        pipeline: &str,
        slot_at: Timestamp,
        outcome: &str,
        run_id: Option<&str>,
    ) -> Result<(), DbError>;

    /// The newest fire of each of the project's Pipelines.
    async fn last_fires(&self, org: &str, project: &str) -> Result<Vec<ScheduleFire>, DbError>;

    /// Delete fires older than `before`; returns how many.
    async fn prune_fires(&self, before: Timestamp) -> Result<u64, DbError>;
}
