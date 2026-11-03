//! `on: cron:` fires (git-bug 5d5fc8a), over in-memory fakes with a fake
//! clock.
//!
//! The index decides when to check; what fires is read from the forge at
//! fire time. Covered: a push to the live default branch indexes a schedule
//! and its slot fires once, across two replicas; a schedule removed, or a
//! default branch renamed, with no webhook is followed at fire time; an
//! unmoved head reads no Pipeline listing; a rate-limited slot is skipped and
//! recorded; missed slots after downtime fire once; two schedules naming one
//! minute make one Run; a Pipeline that no longer compiles is a refused Run.

use std::sync::Arc;

use scarab_engine::{Db, RunSummary, ScheduleStore};
use scarab_forge::{Event, RepoRef};
use scarab_server::schedules::{self, CronScheduler};
use scarab_server::{AppState, LogService};
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb, InMemoryObjectStore, InMemorySchedules};

/// 2026-11-04 10:30:00 UTC.
const T0: i64 = 1_793_788_200_000;
const MIN: i64 = 60_000;
const SHA1: &str = "1111111111111111111111111111111111111111";
const SHA2: &str = "2222222222222222222222222222222222222222";
const PATH: &str = ".scarab/nightly.yaml";

fn hourly(schedule: &str) -> String {
    format!(
        "on: {{ cron: {{ schedule: {schedule} }} }}\nsteps: [{{ id: a, image: busybox, command: [\"true\"] }}]\n"
    )
}

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    }
}

struct H {
    st: AppState,
    db: Arc<InMemoryDb>,
    forge: Arc<FakeForge>,
    clock: Arc<FakeClock>,
    store: Arc<InMemorySchedules>,
    leader: Arc<CronScheduler>,
}

impl H {
    fn new(yaml: &str) -> H {
        let db = Arc::new(InMemoryDb::new());
        let clock = Arc::new(FakeClock::new(T0));
        let forge = Arc::new(
            FakeForge::new()
                .with_file(PATH, yaml.to_string())
                .with_commit("main", SHA1),
        );
        let store = Arc::new(InMemorySchedules::new());
        let logs = Arc::new(LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            db.clone(),
        ));
        let st = AppState::new(db.clone(), clock.clone(), logs)
            .with_forge(forge.clone())
            .with_schedules(store.clone());
        let leader = CronScheduler::new(st.clone(), "replica-a");
        H {
            st,
            db,
            forge,
            clock,
            store,
            leader,
        }
    }

    /// The push webhook for `sha` on `branch`, as the ingest path indexes it.
    async fn push(&self, branch: &str, sha: &str) {
        let event = Event::Push {
            actor: "dev".into(),
            actor_id: String::new(),
            repo: repo(),
            r#ref: format!("refs/heads/{branch}"),
            after: sha.into(),
            message: String::new(),
        };
        let files = schedules::read_pipeline_files(self.forge.as_ref(), &repo(), sha)
            .await
            .unwrap();
        schedules::index_push(
            self.forge.as_ref(),
            self.store.as_ref(),
            self.clock.as_ref(),
            &event,
            &files,
        )
        .await;
    }

    /// One leader tick at `at`, waiting for every task it started.
    async fn tick_at(&self, at: i64) {
        self.clock.advance(at - self.now().await);
        tick(&self.leader).await;
    }

    async fn now(&self) -> i64 {
        scarab_engine::Clock::now(self.clock.as_ref()).await.0
    }

    async fn runs(&self) -> Vec<RunSummary> {
        let mut runs = self.db.list_runs(50).await.unwrap();
        runs.sort_by_key(|r| r.created_at.0);
        runs
    }

    async fn rows(&self) -> Vec<(String, i64, String)> {
        self.store
            .project_schedules("acme", "app")
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.schedule, r.next_fire_at.0, r.source_sha))
            .collect()
    }
}

async fn tick(s: &Arc<CronScheduler>) {
    for task in s.clone().tick().await.unwrap() {
        task.await.unwrap();
    }
}

#[tokio::test]
async fn a_push_to_the_default_branch_indexes_a_schedule_and_its_slot_fires_once() {
    let h = H::new(&hourly("\"0 * * * *\""));
    // Another branch, and a push the default branch has already moved past,
    // write nothing.
    h.push("feature", SHA1).await;
    assert!(h.rows().await.is_empty());
    h.push("main", SHA2).await;
    assert!(h.rows().await.is_empty());

    h.push("main", SHA1).await;
    let eleven = T0 + 30 * MIN;
    assert_eq!(h.rows().await, [("0 * * * *".into(), eleven, SHA1.into())]);

    // Inside the early window the check runs, nothing fires.
    h.tick_at(eleven - 50_000).await;
    assert!(h.runs().await.is_empty());

    h.tick_at(eleven + 2_000).await;
    // A second replica finds the lease held and does nothing.
    tick(&CronScheduler::new(h.st.clone(), "replica-b")).await;
    h.tick_at(eleven + 7_000).await;
    let runs = h.runs().await;
    assert_eq!(runs.len(), 1, "one Run per slot");
    let run = &runs[0];
    assert_eq!(run.trigger_kind.as_deref(), Some("cron"));
    assert_eq!(run.git_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(run.sha.as_deref(), Some(SHA1));
    assert_eq!(run.pipeline.as_deref(), Some("nightly"));
    assert_eq!(run.trigger_title.as_deref(), Some("0 * * * *"));
    assert_eq!(
        h.rows().await[0].1,
        eleven + 60 * MIN,
        "moved to the next slot"
    );
    let fires = h.store.fires();
    assert_eq!(fires.len(), 1);
    assert_eq!(fires[0].outcome, "run");
    assert_eq!(fires[0].run_id.as_deref(), Some(run.run.0.as_str()));
}

#[tokio::test]
async fn a_schedule_removed_without_a_webhook_does_not_fire() {
    let h = H::new(&hourly("\"0 * * * *\""));
    h.push("main", SHA1).await;
    // The schedule is removed in a push whose webhook never arrived.
    h.forge.put_file(
        PATH,
        "on: { push: {} }\nsteps: [{ id: a, image: busybox }]\n",
    );
    h.forge.set_commit("main", SHA2);

    h.tick_at(T0 + 30 * MIN + 1_000).await;
    assert!(h.runs().await.is_empty(), "the old schedule must not fire");
    assert!(
        h.rows().await.is_empty(),
        "the live head declares no schedule"
    );
}

#[tokio::test]
async fn a_renamed_default_branch_is_followed_at_fire_time() {
    let h = H::new(&hourly("\"0 * * * *\""));
    h.push("main", SHA1).await;
    // `main` is renamed `trunk`, with a new commit on it; no webhook.
    h.forge.set_default_branch("trunk");
    h.forge.set_commit("trunk", SHA2);

    h.tick_at(T0 + 30 * MIN + 1_000).await;
    let runs = h.runs().await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].git_ref.as_deref(), Some("refs/heads/trunk"));
    assert_eq!(runs[0].sha.as_deref(), Some(SHA2));
    assert_eq!(
        h.rows().await[0].2,
        SHA2,
        "the index was rewritten from the live head"
    );
}

#[tokio::test]
async fn an_unmoved_head_reads_no_pipeline_listing() {
    let h = H::new(&hourly("\"0 * * * *\""));
    h.push("main", SHA1).await;
    let before = h.forge.file_reads();
    h.tick_at(T0 + 30 * MIN + 1_000).await;
    assert_eq!(h.runs().await.len(), 1);
    // Only the fire's own read of the Pipeline file: no listing, no re-index.
    assert_eq!(h.forge.file_reads() - before, 1);
}

#[tokio::test]
async fn a_rate_limited_slot_is_skipped_and_recorded_then_the_next_one_fires() {
    let h = H::new(&hourly("\"0 * * * *\""));
    h.push("main", SHA1).await;
    let eleven = T0 + 30 * MIN;
    h.forge.set_rate_limited(true);

    h.tick_at(eleven + 2_000).await;
    assert!(h.runs().await.is_empty());
    assert!(
        h.db.refused_runs_for_tenant("acme", "app", 10)
            .await
            .unwrap()
            .is_empty(),
        "still inside the retry window"
    );
    h.tick_at(eleven + 2 * MIN).await;
    let refused =
        h.db.refused_runs_for_tenant("acme", "app", 10)
            .await
            .unwrap();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].trigger_kind, "cron");
    assert_eq!(refused[0].reason_kind, "schedule");
    assert!(
        refused[0].message.contains("rate-limited"),
        "{}",
        refused[0].message
    );
    assert_eq!(h.store.fires()[0].outcome, "missed");
    assert_eq!(h.rows().await[0].1, eleven + 60 * MIN);

    h.forge.set_rate_limited(false);
    h.tick_at(eleven + 60 * MIN + 1_000).await;
    assert_eq!(h.runs().await.len(), 1);
}

#[tokio::test]
async fn slots_missed_while_the_server_was_down_fire_once() {
    let h = H::new(&hourly("\"*/15 * * * *\""));
    h.push("main", SHA1).await;
    // Three hours of slots pass with no tick.
    h.tick_at(T0 + 3 * 60 * MIN + 1_000).await;
    h.tick_at(T0 + 3 * 60 * MIN + 6_000).await;
    assert_eq!(h.runs().await.len(), 1);
    assert_eq!(h.rows().await[0].1, T0 + 3 * 60 * MIN + 15 * MIN);
}

#[tokio::test]
async fn two_schedules_naming_one_minute_make_one_run() {
    let h = H::new(&hourly("[\"0 * * * *\", \"@hourly\"]"));
    h.push("main", SHA1).await;
    assert_eq!(h.rows().await.len(), 2);
    h.tick_at(T0 + 30 * MIN + 1_000).await;
    assert_eq!(h.runs().await.len(), 1);
}

#[tokio::test]
async fn a_pipeline_that_no_longer_compiles_is_a_refused_run() {
    let h = H::new(&hourly("\"0 * * * *\""));
    h.push("main", SHA1).await;
    // The schedule stays; the steps break.
    h.forge.put_file(
        PATH,
        "on: { cron: { schedule: \"0 * * * *\" } }\nsteps: [{ id: a }]\n",
    );
    h.forge.set_commit("main", SHA2);
    h.tick_at(T0 + 30 * MIN + 1_000).await;
    assert!(h.runs().await.is_empty());
    let refused =
        h.db.refused_runs_for_tenant("acme", "app", 10)
            .await
            .unwrap();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].reason_kind, "compile");
    assert_eq!(refused[0].sha.as_deref(), Some(SHA2));
    assert_eq!(h.store.fires()[0].outcome, "refused");
}

#[tokio::test]
async fn the_schedules_endpoint_lists_the_next_and_last_fire() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let h = H::new(&hourly("\"0 * * * *\""));
    h.push("main", SHA1).await;
    h.tick_at(T0 + 30 * MIN + 1_000).await;
    let resp = scarab_server::router(h.st.clone())
        .oneshot(
            Request::get("/v1/repos/acme/app/schedules")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let s = &v["schedules"][0];
    assert_eq!(s["pipeline"], PATH);
    assert_eq!(s["schedule"], "0 * * * *");
    assert_eq!(s["next_fire_at"], T0 + 90 * MIN);
    assert_eq!(s["last_fire"]["outcome"], "run");
    assert_eq!(s["last_fire"]["slot_at"], T0 + 30 * MIN);
}

#[tokio::test]
async fn a_failed_read_at_fire_time_keeps_the_schedule() {
    let h = H::new(&hourly("\"0 * * * *\""));
    h.push("main", SHA1).await;
    let eleven = T0 + 30 * MIN;
    // The head moved without a webhook, and the forge then fails the read
    // with a 502: the schedule must stay, not be read as removed.
    h.forge.set_commit("main", SHA2);
    h.forge.set_read_error(Some("502 Bad Gateway"));
    h.tick_at(eleven + 2_000).await;
    assert!(h.runs().await.is_empty());
    assert_eq!(h.rows().await, [("0 * * * *".into(), eleven, SHA1.into())]);

    h.forge.set_read_error(None);
    h.tick_at(eleven + 20_000).await;
    let runs = h.runs().await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].sha.as_deref(), Some(SHA2));
}

#[tokio::test]
async fn a_check_is_not_reused_after_the_leader_stalled() {
    let h = H::new(&hourly("\"0 * * * *\""));
    h.push("main", SHA1).await;
    let eleven = T0 + 30 * MIN;
    // Checked early for the 11:00 slot, then nothing ticks for two hours
    // while a push the webhook lost moves the head.
    h.tick_at(eleven - 50_000).await;
    h.forge.set_commit("main", SHA2);
    h.forge
        .put_file(PATH, hourly("\"0 * * * *\"").replace("true", "echo moved"));
    h.tick_at(eleven + 2 * 60 * MIN).await;
    let runs = h.runs().await;
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].sha.as_deref(),
        Some(SHA2),
        "fired from a stale check"
    );
}

#[tokio::test]
async fn the_schedule_whose_when_matches_makes_the_run() {
    // Both schedules name 11:00; only the hourly one passes `when:`. The
    // `*/30` one sorts first and must not use up the slot.
    let yaml = "on: { cron: { schedule: [\"*/30 * * * *\", \"0 * * * *\"], when: \"event.schedule == '0 * * * *'\" } }\nsteps: [{ id: a, image: busybox, command: [\"true\"] }]\n";
    let h = H::new(yaml);
    h.push("main", SHA1).await;
    h.tick_at(T0 + 30 * MIN + 1_000).await;
    let runs = h.runs().await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].trigger_title.as_deref(), Some("0 * * * *"));
}
