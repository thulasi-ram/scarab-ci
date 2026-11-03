//! `on: cron:` fires (git-bug 5d5fc8a).
//!
//! The schedule index ([`ScheduleStore`]) decides only **when** to check.
//! What fires is read from the forge at fire time, every time:
//!
//! 1. the repo's default branch, as the forge reports it now (never a stored
//!    value), so a renamed or switched default branch is followed;
//! 2. that branch's head;
//! 3. when the head moved since the index was written, every Pipeline file at
//!    the new head, which rewrites the project's index (refresh on fire).
//!
//! A slot fires only if the Pipeline at the live head still declares that
//! schedule. A removed or edited schedule, or a deleted or renamed file, is
//! dropped from the index and starts nothing. So a lost webhook can cost a
//! missing Run (a new schedule nobody pushed to the default branch since),
//! never a Run of old config or on an old branch.
//!
//! Rate limits: the check starts up to [`CHECK_EARLY_MS`] before the slot so
//! the top of the hour is spread out; checks and fires for one owner (one
//! forge installation) run at most [`PER_OWNER_CHECKS`] at a time; the forge
//! GETs are conditional (the adapters keep `ETag`s); and an owner the forge
//! rate-limited is left alone until the forge's reset. A slot whose check did
//! not succeed by [`MISS_AFTER_MS`] after it is skipped and recorded as a
//! refused Run, so it is visible where the Run would have been.
//!
//! One leader runs the loop (the [`CRON_LEASE`] lease), and each slot is
//! claimed in the store before its Run is made, so two replicas, or two
//! schedules of one Pipeline naming the same minute, make one Run.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use utoipa::ToSchema;

use scarab_engine::{Clock, ScheduleEntry, ScheduleFire, ScheduleStore, Timestamp};
use scarab_forge::{Event, ForgeError, ForgePort, RepoRef};
use scarab_identity::Action;
use scarab_pipeline::cron::Schedule;

use crate::{
    admit_and_create_run, authorize_scoped, installed_libraries, is_pipeline_file,
    prefetch_libs_and_compile, record_compile_failure, record_refusal, Admission, ApiError,
    AppState, CompileErrorDto, TriggerError, CONFIG_DIR,
};

/// The lease that makes one replica the cron leader.
pub const CRON_LEASE: &str = "cron-scheduler";
const LEASE_TTL_MS: i64 = 30_000;
/// How often the leader looks for due slots.
pub const TICK_MS: u64 = 5_000;
/// How long before its slot a schedule's check may run.
pub const CHECK_EARLY_MS: i64 = 60_000;
/// How long after its slot a failed check may be retried before the slot is
/// skipped as missed.
pub const MISS_AFTER_MS: i64 = 60_000;
/// The wait before a project whose check failed is checked again.
pub const RETRY_AFTER_ERROR_MS: i64 = 10_000;
/// Checks and fires in flight at once for one owner.
pub const PER_OWNER_CHECKS: usize = 4;
/// The wait after a rate limit that named no reset time.
const DEFAULT_BACKOFF_MS: i64 = 60_000;
/// Due rows read per tick.
const DUE_BATCH: u32 = 500;
/// How long fired slots are kept: the schedules list shows the last one.
const FIRES_KEPT_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// How often old fired slots are pruned.
const PRUNE_EVERY_MS: i64 = 60 * 60 * 1000;

/// The schedule rows `files` (each a `.scarab/` path and its YAML, read at
/// `sha`) declare, each with its next slot after `now`.
pub fn entries_from_files(
    repo: &RepoRef,
    sha: &str,
    files: &[(String, String)],
    now: Timestamp,
) -> Vec<ScheduleEntry> {
    let mut out = Vec::new();
    for (path, yaml) in files {
        for expr in scarab_pipeline::cron_schedules_of(yaml) {
            let Some(next) = Schedule::parse(&expr)
                .ok()
                .and_then(|s| s.next_after(now.0))
            else {
                continue;
            };
            out.push(ScheduleEntry {
                org: repo.owner.clone(),
                project: repo.name.clone(),
                pipeline: path.clone(),
                schedule: expr,
                next_fire_at: Timestamp(next),
                source_sha: sha.to_string(),
            });
        }
    }
    out
}

/// Every Pipeline file under `.scarab/` at `sha`, with its YAML. A listing
/// that fails is an error, never an empty list: an empty list would clear
/// the project's index. A file that vanished between the listing and the
/// read, or is not UTF-8, is left out.
pub async fn read_pipeline_files(
    forge: &dyn ForgePort,
    repo: &RepoRef,
    sha: &str,
) -> Result<Vec<(String, String)>, ForgeError> {
    let mut paths: Vec<String> = forge
        .list_dir_at_ref(repo, sha, CONFIG_DIR)
        .await?
        .into_iter()
        .filter(|p| is_pipeline_file(p))
        .collect();
    paths.sort();
    let mut files = Vec::new();
    for path in paths {
        match forge.read_file_at_ref(repo, sha, &path).await {
            Ok(bytes) => {
                if let Ok(yaml) = String::from_utf8(bytes) {
                    files.push((path, yaml));
                }
            }
            Err(ForgeError::Api(_)) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(files)
}

/// Rewrite the project's index from a push, when the push is the head of the
/// live default branch. `files` are the push's Pipeline files, already read
/// at its commit. A push to another branch, or one a newer push has already
/// passed, writes nothing. Best effort: a failure is logged, and the next
/// push or fire corrects the index.
pub async fn index_push(
    forge: &dyn ForgePort,
    store: &dyn ScheduleStore,
    clock: &dyn Clock,
    event: &Event,
    files: &[(String, String)],
) {
    let Event::Push {
        repo, r#ref, after, ..
    } = event
    else {
        return;
    };
    let Some(branch) = r#ref.strip_prefix("refs/heads/") else {
        return;
    };
    let declares = files
        .iter()
        .any(|(_, yaml)| !scarab_pipeline::cron_schedules_of(yaml).is_empty());
    let result: Result<(), String> = async {
        // Nothing to add and nothing to remove: no forge call.
        if !declares
            && store
                .project_schedules(&repo.owner, &repo.name)
                .await
                .map_err(|e| e.to_string())?
                .is_empty()
        {
            return Ok(());
        }
        let default = forge
            .default_branch(repo)
            .await
            .map_err(|e| e.to_string())?;
        if default != branch {
            return Ok(());
        }
        let head = forge
            .latest_commit(repo, branch)
            .await
            .map_err(|e| e.to_string())?;
        if head.sha != *after {
            return Ok(());
        }
        let entries = entries_from_files(repo, after, files, clock.now().await);
        store
            .replace_project_schedules(&repo.owner, &repo.name, after, &entries)
            .await
            .map_err(|e| e.to_string())
    }
    .await;
    if let Err(e) = result {
        tracing::warn!(
            org = %repo.owner,
            project = %repo.name,
            "cron: the schedule index was not updated from this push: {e}"
        );
    }
}

/// What a fire came to, as [`ScheduleFire::outcome`] names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireOutcome {
    Run(String),
    Refused,
    Skipped,
}

impl FireOutcome {
    fn as_str(&self) -> &'static str {
        match self {
            FireOutcome::Run(_) => "run",
            FireOutcome::Refused => "refused",
            FireOutcome::Skipped => "skipped",
        }
    }
}

/// Start the Run of one Pipeline for one slot, at `sha` on `branch` (both
/// just read from the forge). The shared read → compile → admission path,
/// as a dispatch takes it: a Pipeline that does not compile, a launch input
/// with no default, or an admission refusal is a refused Run; a `when:` on
/// the cron trigger or step guards that leave nothing to run start nothing.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_cron(
    st: &AppState,
    forge: &dyn ForgePort,
    repo: &RepoRef,
    branch: &str,
    sha: &str,
    pipeline: &str,
    schedule: &str,
) -> Result<FireOutcome, TriggerError> {
    let db = st.db.as_ref();
    let clock = st.clock.as_ref();
    let ports = st.admission_ports();
    let event = Event::Cron {
        repo: repo.clone(),
        r#ref: format!("refs/heads/{branch}"),
        sha: sha.to_string(),
        schedule: schedule.to_string(),
    };
    let refuse = |message: String| {
        let errors = vec![CompileErrorDto::other(message.clone())];
        let event = event.clone();
        async move {
            record_refusal(db, clock, &event, pipeline, "schedule", message, &errors).await;
            Ok(FireOutcome::Refused)
        }
    };
    let yaml = match forge.read_file_at_ref(repo, sha, pipeline).await {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(yaml) => yaml,
            Err(_) => return refuse("the Pipeline file is not UTF-8".into()).await,
        },
        // Gone since the check listed it: the schedule went with it.
        Err(ForgeError::Api(_)) => return Ok(FireOutcome::Skipped),
        Err(e) => return refuse(format!("the Pipeline could not be read: {e}")).await,
    };
    let installed = installed_libraries(ports.libraries, ports.plugins, &repo.owner).await?;
    let compiled = prefetch_libs_and_compile(forge, repo, sha, &yaml, &installed).await;
    let ir = match record_compile_failure(db, clock, &event, pipeline, compiled).await {
        Ok(ir) => ir,
        Err(TriggerError::Pipeline(_)) => return Ok(FireOutcome::Refused),
        Err(e) => return refuse(format!("the Pipeline could not be compiled: {e}")).await,
    };
    // The compiled Pipeline is the authority on what it declares.
    if !scarab_pipeline::cron_schedules(&ir)
        .iter()
        .any(|s| s.trim() == schedule)
    {
        return Ok(FireOutcome::Skipped);
    }
    let ctx = event.context();
    if !scarab_pipeline::matches_trigger(&ir, "cron", &ctx)
        .map_err(|e| TriggerError::Pipeline(e.to_string()))?
    {
        return Ok(FireOutcome::Skipped);
    }
    let params = match scarab_pipeline::params::resolve_params(&ir.interface, &BTreeMap::new()) {
        Ok(p) => p,
        Err(e) => {
            return refuse(format!(
                "a cron Run takes no launch inputs, so every input needs a default: {e}"
            ))
            .await
        }
    };
    let excluded = scarab_pipeline::excluded_steps_with_params(&ir, &ctx, &params)
        .map_err(|e| TriggerError::Pipeline(e.to_string()))?;
    if excluded.len() == ir.steps.len() {
        return Ok(FireOutcome::Skipped);
    }
    match admit_and_create_run(
        forge,
        db,
        clock,
        ports,
        &event,
        &ir,
        pipeline,
        &excluded,
        Some(&params),
    )
    .await?
    {
        Admission::Created(run) => Ok(FireOutcome::Run(run.0)),
        // Recorded by admission, except a branch-scoped Environment that does
        // not admit the default branch, which is the rule working.
        Admission::Refused(_) => Ok(FireOutcome::Refused),
    }
}

type ProjectKey = (String, String);

/// The last successful check of a project's default branch.
#[derive(Debug, Clone)]
struct Checked {
    at: i64,
    branch: String,
    sha: String,
}

#[derive(Default)]
struct LoopState {
    /// Projects with a task running.
    inflight: HashSet<ProjectKey>,
    /// The last successful check per project: reused for a slot it ran at
    /// most [`CHECK_EARLY_MS`] before.
    checked: HashMap<ProjectKey, Checked>,
    /// A project whose check failed is not checked again before this.
    retry_at: HashMap<ProjectKey, i64>,
    /// An owner the forge rate-limited is not called before this.
    backoff_until: HashMap<String, i64>,
    last_prune: i64,
}

/// The cron leader's loop state. One per replica; only the lease holder acts.
pub struct CronScheduler {
    st: AppState,
    owner: String,
    state: Mutex<LoopState>,
    owners: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl CronScheduler {
    pub fn new(st: AppState, owner: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            st,
            owner: owner.into(),
            state: Mutex::new(LoopState::default()),
            owners: Mutex::new(HashMap::new()),
        })
    }

    /// Run [`tick`](Self::tick) every [`TICK_MS`] until the task is dropped.
    pub fn spawn(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.clone().tick().await {
                    tracing::warn!("cron: tick failed: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_millis(TICK_MS)).await;
            }
        })
    }

    fn semaphore(&self, owner: &str) -> Arc<Semaphore> {
        self.owners
            .lock()
            .unwrap()
            .entry(owner.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(PER_OWNER_CHECKS)))
            .clone()
    }

    /// One pass: take or renew the lease, then start a task for each project
    /// with a slot due within [`CHECK_EARLY_MS`]. Returns the tasks, so a test
    /// can wait for them; the loop does not.
    pub async fn tick(self: Arc<Self>) -> Result<Vec<JoinHandle<()>>, String> {
        let (Some(store), Some(_)) = (self.st.schedules.clone(), self.st.forge.as_ref()) else {
            return Ok(Vec::new());
        };
        let lease = self
            .st
            .db
            .lease(CRON_LEASE, &self.owner, LEASE_TTL_MS)
            .await
            .map_err(|e| e.to_string())?;
        if lease.owner != self.owner {
            // Another replica leads; what this one checked may go stale.
            self.state.lock().unwrap().checked.clear();
            return Ok(Vec::new());
        }
        let now = self.st.clock.now().await.0;
        let prune = {
            let mut s = self.state.lock().unwrap();
            let due = now - s.last_prune >= PRUNE_EVERY_MS;
            if due {
                s.last_prune = now;
            }
            due
        };
        if prune {
            if let Err(e) = store.prune_fires(Timestamp(now - FIRES_KEPT_MS)).await {
                tracing::warn!("cron: pruning old fires failed: {e}");
            }
        }
        let due = store
            .due_schedules(Timestamp(now + CHECK_EARLY_MS), DUE_BATCH)
            .await
            .map_err(|e| e.to_string())?;
        let mut groups: BTreeMap<ProjectKey, Vec<ScheduleEntry>> = BTreeMap::new();
        for row in due {
            groups
                .entry((row.org.clone(), row.project.clone()))
                .or_default()
                .push(row);
        }
        let mut tasks = Vec::new();
        for (key, rows) in groups {
            let backed_off = {
                let mut s = self.state.lock().unwrap();
                if s.inflight.contains(&key) || s.retry_at.get(&key).is_some_and(|t| *t > now) {
                    continue;
                }
                let backed_off = s.backoff_until.get(&key.0).is_some_and(|t| *t > now);
                s.inflight.insert(key.clone());
                backed_off
            };
            let me = self.clone();
            tasks.push(tokio::spawn(async move {
                if backed_off {
                    me.miss_overdue(&rows, now, "the forge rate-limited Scarab")
                        .await;
                } else {
                    me.run_project(&key, rows, now).await;
                }
                me.state.lock().unwrap().inflight.remove(&key);
            }));
        }
        Ok(tasks)
    }

    /// Check one project (or reuse a check made for this slot), then fire
    /// each of its rows that is due.
    async fn run_project(&self, key: &ProjectKey, rows: Vec<ScheduleEntry>, now: i64) {
        let repo = RepoRef {
            owner: key.0.clone(),
            name: key.1.clone(),
        };
        let earliest = rows.iter().map(|r| r.next_fire_at.0).min().unwrap_or(now);
        let reuse = self
            .state
            .lock()
            .unwrap()
            .checked
            .get(key)
            .filter(|c| c.at >= earliest - CHECK_EARLY_MS && c.sha == rows[0].source_sha)
            .cloned();
        let sem = self.semaphore(&repo.owner);
        let (checked, rows) = match reuse {
            Some(c) => (c, rows),
            None => {
                let _permit = sem.acquire().await.expect("never closed");
                match self.check(&repo, &rows, now).await {
                    Ok(Some((c, rows))) => {
                        let mut s = self.state.lock().unwrap();
                        s.checked.insert(key.clone(), c.clone());
                        s.retry_at.remove(key);
                        (c, rows)
                    }
                    // Unbound: its rows were dropped.
                    Ok(None) => return,
                    Err(e) => {
                        self.check_failed(key, &rows, now, e).await;
                        return;
                    }
                }
            }
        };
        let now = self.st.clock.now().await.0;
        for row in rows.iter().filter(|r| r.next_fire_at.0 <= now) {
            let _permit = sem.acquire().await.expect("never closed");
            self.fire(&repo, &checked, row, now).await;
        }
    }

    /// Read the default branch and its head; when the head moved since the
    /// rows were written, re-read every Pipeline file there and rewrite the
    /// project's index. `None` when the repo is no longer bound (its rows are
    /// dropped).
    async fn check(
        &self,
        repo: &RepoRef,
        rows: &[ScheduleEntry],
        now: i64,
    ) -> Result<Option<(Checked, Vec<ScheduleEntry>)>, ForgeError> {
        let store = self.st.schedules.clone().expect("checked by tick");
        let forge = self.st.forge.clone().expect("checked by tick");
        let db_err = |e: scarab_engine::DbError| ForgeError::Api(format!("database: {e}"));
        if let Some(connections) = &self.st.connections {
            let bound = connections
                .resolve(repo)
                .await
                .map_err(|e| ForgeError::Api(format!("registry: {e}")))?;
            if bound.is_none() {
                store
                    .replace_project_schedules(&repo.owner, &repo.name, "", &[])
                    .await
                    .map_err(db_err)?;
                return Ok(None);
            }
        }
        let branch = forge.default_branch(repo).await?;
        let head = forge.latest_commit(repo, &branch).await?;
        if !scarab_forge::is_commit_sha(&head.sha) {
            return Err(ForgeError::Api(
                "the forge resolved the default branch to a value that is not a commit SHA".into(),
            ));
        }
        let moved = rows.first().is_none_or(|r| r.source_sha != head.sha);
        if moved {
            let files = read_pipeline_files(forge.as_ref(), repo, &head.sha).await?;
            let entries = entries_from_files(repo, &head.sha, &files, Timestamp(now));
            store
                .replace_project_schedules(&repo.owner, &repo.name, &head.sha, &entries)
                .await
                .map_err(db_err)?;
        }
        let rows = store
            .project_schedules(&repo.owner, &repo.name)
            .await
            .map_err(db_err)?;
        Ok(Some((
            Checked {
                at: now,
                branch,
                sha: head.sha,
            },
            rows,
        )))
    }

    /// A check failed: try again after [`RETRY_AFTER_ERROR_MS`] (or the
    /// forge's reset, for a rate limit), and skip the slots already past
    /// [`MISS_AFTER_MS`].
    async fn check_failed(
        &self,
        key: &ProjectKey,
        rows: &[ScheduleEntry],
        now: i64,
        e: ForgeError,
    ) {
        tracing::warn!(org = %key.0, project = %key.1, "cron: checking the default branch failed: {e}");
        {
            let mut s = self.state.lock().unwrap();
            s.retry_at.insert(key.clone(), now + RETRY_AFTER_ERROR_MS);
            if let ForgeError::RateLimited {
                retry_after_secs, ..
            } = &e
            {
                let wait = retry_after_secs.map_or(DEFAULT_BACKOFF_MS, |s| s as i64 * 1000);
                s.backoff_until.insert(key.0.clone(), now + wait);
            }
        }
        let why = match &e {
            ForgeError::RateLimited { .. } => "the forge rate-limited Scarab".to_string(),
            other => format!("the forge could not be read: {other}"),
        };
        self.miss_overdue(rows, now, &why).await;
    }

    /// Skip each row whose slot is more than [`MISS_AFTER_MS`] past: claim it
    /// as `missed`, record a refused Run saying why, and move the row on.
    async fn miss_overdue(&self, rows: &[ScheduleEntry], now: i64, why: &str) {
        let store = self.st.schedules.clone().expect("checked by tick");
        for row in rows
            .iter()
            .filter(|r| r.next_fire_at.0 + MISS_AFTER_MS < now)
        {
            let fire = ScheduleFire {
                org: row.org.clone(),
                project: row.project.clone(),
                pipeline: row.pipeline.clone(),
                schedule: row.schedule.clone(),
                slot_at: row.next_fire_at,
                fired_at: Timestamp(now),
                outcome: "missed".into(),
                run_id: None,
            };
            match store.claim_fire(&fire).await {
                Ok(true) => {
                    let message = format!(
                        "the slot was skipped: {why}, so the default branch could not be read \
                         in time"
                    );
                    let refused = scarab_engine::RefusedRun {
                        id: 0,
                        org: row.org.clone(),
                        project: row.project.clone(),
                        pipeline: row.pipeline.clone(),
                        trigger_kind: "cron".into(),
                        actor: None,
                        git_ref: None,
                        sha: None,
                        title: Some(row.schedule.clone()),
                        reason_kind: "schedule".into(),
                        errors: serde_json::to_value(vec![CompileErrorDto::other(message.clone())])
                            .unwrap_or_default(),
                        message,
                        created_at: Timestamp(now),
                    };
                    if let Err(e) = self.st.db.record_refused_run(&refused).await {
                        tracing::warn!("cron: recording a missed slot failed: {e}");
                    }
                }
                Ok(false) => {}
                Err(e) => tracing::warn!("cron: claiming a missed slot failed: {e}"),
            }
            self.advance(row, now).await;
        }
    }

    /// Claim the row's slot, make its Run, record the outcome, and move the
    /// row to its next slot after `now`.
    async fn fire(&self, repo: &RepoRef, checked: &Checked, row: &ScheduleEntry, now: i64) {
        let store = self.st.schedules.clone().expect("checked by tick");
        let forge = self.st.forge.clone().expect("checked by tick");
        let fire = ScheduleFire {
            org: row.org.clone(),
            project: row.project.clone(),
            pipeline: row.pipeline.clone(),
            schedule: row.schedule.clone(),
            slot_at: row.next_fire_at,
            fired_at: Timestamp(now),
            outcome: "pending".into(),
            run_id: None,
        };
        match store.claim_fire(&fire).await {
            Ok(true) => {
                let outcome = match dispatch_cron(
                    &self.st,
                    forge.as_ref(),
                    repo,
                    &checked.branch,
                    &checked.sha,
                    &row.pipeline,
                    &row.schedule,
                )
                .await
                {
                    Ok(o) => o,
                    Err(e) => {
                        tracing::warn!(
                            org = %row.org,
                            project = %row.project,
                            pipeline = %row.pipeline,
                            "cron: the fire failed: {e}"
                        );
                        FireOutcome::Refused
                    }
                };
                let run = match &outcome {
                    FireOutcome::Run(id) => Some(id.as_str()),
                    _ => None,
                };
                if let Err(e) = store
                    .finish_fire(
                        &row.org,
                        &row.project,
                        &row.pipeline,
                        row.next_fire_at,
                        outcome.as_str(),
                        run,
                    )
                    .await
                {
                    tracing::warn!("cron: recording a fire failed: {e}");
                }
            }
            // Claimed already: another replica, or another schedule of this
            // Pipeline naming the same minute.
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("cron: claiming a slot failed: {e}");
                return;
            }
        }
        self.advance(row, now).await;
    }

    /// Move the row from its slot to the first slot after `now` (after a
    /// downtime the missed slots fire once, not once each).
    async fn advance(&self, row: &ScheduleEntry, now: i64) {
        let store = self.st.schedules.clone().expect("checked by tick");
        let next = Schedule::parse(&row.schedule)
            .ok()
            .and_then(|s| s.next_after(now.max(row.next_fire_at.0)))
            .unwrap_or(i64::MAX);
        if let Err(e) = store
            .advance_schedule(row, row.next_fire_at, Timestamp(next))
            .await
        {
            tracing::warn!("cron: moving a schedule to its next slot failed: {e}");
        }
    }
}

/// One schedule, with its Pipeline's last fire.
#[derive(Debug, Serialize, ToSchema)]
pub struct ScheduleDto {
    /// The pipeline file, `.scarab/<name>.yaml`.
    pub pipeline: String,
    /// The cron expression as written, in UTC.
    pub schedule: String,
    /// The next slot, Unix milliseconds. The default branch is read again
    /// then; a schedule removed since does not fire.
    pub next_fire_at: i64,
    /// The default-branch commit this schedule was read from.
    pub source_sha: String,
    /// The Pipeline's last claimed slot, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fire: Option<ScheduleFireDto>,
}

/// A claimed slot.
#[derive(Debug, Serialize, ToSchema)]
pub struct ScheduleFireDto {
    /// The schedule that fired.
    pub schedule: String,
    /// The slot, Unix milliseconds.
    pub slot_at: i64,
    /// `pending`, `run`, `refused` (see the refused runs), `skipped` (its
    /// `when:` or step guards left nothing to run) or `missed` (the forge
    /// could not be read in time).
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// `GET /v1/repos/{org}/{repo}/schedules` body.
#[derive(Debug, Serialize, ToSchema)]
pub struct ScheduleListResponse {
    pub schedules: Vec<ScheduleDto>,
}

/// A Project's `on: cron:` schedules, as last read from its default branch,
/// with each one's next slot and its Pipeline's last fire. Read on the
/// Project.
#[utoipa::path(
    get,
    path = "/v1/repos/{org}/{repo}/schedules",
    params(
        ("org" = String, Path, description = "org slug"),
        ("repo" = String, Path, description = "project (repo) name")
    ),
    summary = "The Project's cron schedules and their next and last fires",
    responses((status = 200, body = ScheduleListResponse))
)]
pub(crate) async fn list_schedules(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
) -> Result<Json<ScheduleListResponse>, ApiError> {
    let scope = scarab_identity::Scope::Project {
        org: org.clone(),
        name: repo.clone(),
    };
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let Some(store) = st.schedules.clone() else {
        return Ok(Json(ScheduleListResponse {
            schedules: Vec::new(),
        }));
    };
    let rows = store.project_schedules(&org, &repo).await?;
    let last: HashMap<String, ScheduleFire> = store
        .last_fires(&org, &repo)
        .await?
        .into_iter()
        .map(|f| (f.pipeline.clone(), f))
        .collect();
    let schedules = rows
        .into_iter()
        .map(|r| ScheduleDto {
            last_fire: last.get(&r.pipeline).map(|f| ScheduleFireDto {
                schedule: f.schedule.clone(),
                slot_at: f.slot_at.0,
                outcome: f.outcome.clone(),
                run_id: f.run_id.clone(),
            }),
            pipeline: r.pipeline,
            schedule: r.schedule,
            next_fire_at: r.next_fire_at.0,
            source_sha: r.source_sha,
        })
        .collect();
    Ok(Json(ScheduleListResponse { schedules }))
}
