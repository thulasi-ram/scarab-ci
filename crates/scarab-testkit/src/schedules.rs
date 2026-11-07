//! [`InMemorySchedules`]: the `ScheduleStore` port (git-bug 5d5fc8a) without
//! a database. The contract suite in `scarab-db-postgres/tests` runs it and
//! the Postgres adapter against the same assertions.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use scarab_engine::{DbError, ScheduleEntry, ScheduleFire, ScheduleStore, Timestamp};

type EntryKey = (String, String, String, String);
type FireKey = (String, String, String, i64);

#[derive(Default)]
pub struct InMemorySchedules {
    entries: Mutex<BTreeMap<EntryKey, ScheduleEntry>>,
    fires: Mutex<BTreeMap<FireKey, ScheduleFire>>,
}

impl InMemorySchedules {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every fire kept, in key order: a test reads what each slot came to.
    pub fn fires(&self) -> Vec<ScheduleFire> {
        self.fires.lock().unwrap().values().cloned().collect()
    }
}

fn key(e: &ScheduleEntry) -> EntryKey {
    (
        e.org.clone(),
        e.project.clone(),
        e.pipeline.clone(),
        e.schedule.clone(),
    )
}

#[async_trait]
impl ScheduleStore for InMemorySchedules {
    async fn replace_project_schedules(
        &self,
        org: &str,
        project: &str,
        source_sha: &str,
        entries: &[ScheduleEntry],
    ) -> Result<(), DbError> {
        let mut map = self.entries.lock().unwrap();
        let mut kept = BTreeMap::new();
        for e in entries {
            let k = (
                org.to_string(),
                project.to_string(),
                e.pipeline.clone(),
                e.schedule.clone(),
            );
            let next = map.get(&k).map_or(e.next_fire_at, |old| old.next_fire_at);
            kept.insert(
                k,
                ScheduleEntry {
                    org: org.to_string(),
                    project: project.to_string(),
                    pipeline: e.pipeline.clone(),
                    schedule: e.schedule.clone(),
                    next_fire_at: next,
                    source_sha: source_sha.to_string(),
                },
            );
        }
        map.retain(|(o, p, _, _), _| o != org || p != project);
        map.extend(kept);
        Ok(())
    }

    async fn project_schedules(
        &self,
        org: &str,
        project: &str,
    ) -> Result<Vec<ScheduleEntry>, DbError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.org == org && e.project == project)
            .cloned()
            .collect())
    }

    async fn due_schedules(
        &self,
        before: Timestamp,
        after: Option<&ScheduleEntry>,
        limit: u32,
    ) -> Result<Vec<ScheduleEntry>, DbError> {
        let cursor = after.map(|e| (e.next_fire_at.0, key(e)));
        let mut due: Vec<ScheduleEntry> = self
            .entries
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.next_fire_at.0 <= before.0)
            .filter(|e| {
                cursor
                    .as_ref()
                    .is_none_or(|c| (e.next_fire_at.0, key(e)) > *c)
            })
            .cloned()
            .collect();
        due.sort_by_key(|e| (e.next_fire_at.0, key(e)));
        due.truncate(limit as usize);
        Ok(due)
    }

    async fn advance_schedule(
        &self,
        e: &ScheduleEntry,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<bool, DbError> {
        let mut map = self.entries.lock().unwrap();
        match map.get_mut(&key(e)) {
            Some(row) if row.next_fire_at == from => {
                row.next_fire_at = to;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn claim_fire(&self, f: &ScheduleFire) -> Result<bool, DbError> {
        let k = (
            f.org.clone(),
            f.project.clone(),
            f.pipeline.clone(),
            f.slot_at.0,
        );
        let mut fires = self.fires.lock().unwrap();
        if fires.contains_key(&k) {
            return Ok(false);
        }
        fires.insert(k, f.clone());
        Ok(true)
    }

    async fn finish_fire(
        &self,
        org: &str,
        project: &str,
        pipeline: &str,
        slot_at: Timestamp,
        outcome: &str,
        run_id: Option<&str>,
    ) -> Result<(), DbError> {
        let k = (
            org.to_string(),
            project.to_string(),
            pipeline.to_string(),
            slot_at.0,
        );
        if let Some(f) = self.fires.lock().unwrap().get_mut(&k) {
            f.outcome = outcome.to_string();
            f.run_id = run_id.map(str::to_string);
        }
        Ok(())
    }

    async fn last_fires(&self, org: &str, project: &str) -> Result<Vec<ScheduleFire>, DbError> {
        let mut last: BTreeMap<String, ScheduleFire> = BTreeMap::new();
        for f in self.fires.lock().unwrap().values() {
            if f.org != org || f.project != project {
                continue;
            }
            match last.get(&f.pipeline) {
                Some(seen) if seen.slot_at.0 >= f.slot_at.0 => {}
                _ => {
                    last.insert(f.pipeline.clone(), f.clone());
                }
            }
        }
        Ok(last.into_values().collect())
    }

    async fn prune_fires(&self, before: Timestamp) -> Result<u64, DbError> {
        let mut fires = self.fires.lock().unwrap();
        let n = fires.len();
        fires.retain(|_, f| f.fired_at.0 >= before.0);
        Ok((n - fires.len()) as u64)
    }
}
