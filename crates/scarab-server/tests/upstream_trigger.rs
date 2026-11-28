//! `on: upstream:` (git-bug 149b4db): a finished Run starts the pipelines in
//! the same repository that follow it, at the same commit. The forge is a
//! FakeForge and the store InMemoryDb (no network).

use scarab_engine::ports::{ExecState, FailureClass};
use scarab_engine::{Db, RunId, RunStatus, RunSummary, Scheduler};
use scarab_forge::{Event, RepoRef};
use scarab_server::followups::drain_followups;
use scarab_server::{trigger_run_from_event, trigger_runs_from_event, AdmissionPorts};
use scarab_testkit::{FakeClock, FakeExecutor, FakeForge, InMemoryDb};

const CI: &str = r#"
on:
  push: {}
  pull_request: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

const DEPLOY: &str = r#"
on:
  upstream: { pipeline: ci }
steps:
  - { id: ship, image: busybox, command: ["true"] }
"#;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    }
}

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "build the thing".into(),
        before: String::new(),
        changed_paths: Default::default(),
    }
}

struct World {
    forge: FakeForge,
    db: InMemoryDb,
    clock: FakeClock,
    exec: FakeExecutor,
}

impl World {
    fn new(files: &[(&str, &str)]) -> Self {
        let mut forge = FakeForge::new();
        for (path, yaml) in files {
            forge = forge.with_file(*path, *yaml);
        }
        Self {
            forge,
            db: InMemoryDb::new(),
            clock: FakeClock::new(1_000),
            exec: FakeExecutor::new(),
        }
    }

    async fn start(&self, event: &Event) -> Vec<RunId> {
        trigger_runs_from_event(
            &self.forge,
            &self.db,
            &self.clock,
            AdmissionPorts::governance(None),
            event,
        )
        .await
        .unwrap()
        .runs
    }

    /// Drive `run` to its end, its one Step ending in `outcome`.
    async fn finish(&self, run: &RunId, outcome: ExecState) {
        self.exec.script_outcome(outcome);
        let sched = Scheduler::new(&self.db, &self.clock, &self.exec, "sched");
        for _ in 0..10 {
            sched.tick(run).await.unwrap();
            if self
                .db
                .run_status(run)
                .await
                .unwrap()
                .unwrap()
                .is_terminal()
            {
                return;
            }
        }
        panic!("run did not settle");
    }

    async fn drain(&self) -> usize {
        drain_followups(
            &self.forge,
            &self.db,
            &self.clock,
            AdmissionPorts::governance(None),
            "followups",
            32,
            30_000,
        )
        .await
        .unwrap()
    }

    async fn runs(&self) -> Vec<RunSummary> {
        self.db.list_runs(50).await.unwrap()
    }

    async fn runs_of(&self, pipeline: &str) -> Vec<RunSummary> {
        self.runs()
            .await
            .into_iter()
            .filter(|r| r.pipeline.as_deref() == Some(pipeline))
            .collect()
    }
}

fn failed() -> ExecState {
    ExecState::Failed {
        exit_code: Some(1),
        class: FailureClass::Step,
        cause: None,
    }
}

#[tokio::test]
async fn a_finished_run_starts_its_follow_up_at_the_same_commit() {
    let w = World::new(&[(".scarab/ci.yaml", CI), (".scarab/deploy.yaml", DEPLOY)]);
    let runs = w.start(&push()).await;
    assert_eq!(runs.len(), 1, "only ci starts on the push");
    // Nothing follows a Run that has not finished.
    assert_eq!(w.drain().await, 0);
    w.finish(&runs[0], ExecState::Succeeded).await;

    assert_eq!(w.drain().await, 1);
    let deploy = w.runs_of("deploy").await;
    assert_eq!(deploy.len(), 1);
    let d = &deploy[0];
    assert_eq!(d.trigger_kind.as_deref(), Some("upstream"));
    assert_eq!(
        d.sha.as_deref(),
        Some(SHA),
        "the follow-up builds ci's commit"
    );
    assert_eq!(d.git_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(
        d.actor.as_deref(),
        Some("octocat"),
        "the chain keeps its actor"
    );

    // The message is retired: a second drain starts nothing.
    assert_eq!(w.drain().await, 0);
    assert_eq!(w.runs_of("deploy").await.len(), 1);
}

#[tokio::test]
async fn the_same_upstream_event_twice_starts_one_run() {
    // Delivery is at-least-once; a redelivered `run_finished` must not
    // start a second deploy.
    let w = World::new(&[(".scarab/deploy.yaml", DEPLOY)]);
    let event = Event::Upstream {
        repo: repo(),
        run: "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b".into(),
        generation: 0,
        pipeline: "ci".into(),
        status: "succeeded".into(),
        r#ref: "refs/heads/main".into(),
        sha: SHA.into(),
        actor: String::new(),
        chain: vec!["ci".into()],
    };
    assert_eq!(w.start(&event).await.len(), 1);
    assert!(w.start(&event).await.is_empty(), "already started");
    assert_eq!(w.runs_of("deploy").await.len(), 1);
}

#[tokio::test]
async fn a_failed_upstream_starts_only_what_follows_failures() {
    const ON_FAILURE: &str = r#"
on:
  upstream: { pipeline: ci, status: failed }
steps:
  - { id: page, image: busybox, command: ["true"] }
"#;
    let w = World::new(&[
        (".scarab/ci.yaml", CI),
        (".scarab/deploy.yaml", DEPLOY),
        (".scarab/page.yaml", ON_FAILURE),
    ]);
    let runs = w.start(&push()).await;
    w.finish(&runs[0], failed()).await;
    assert_eq!(w.drain().await, 1);
    assert!(
        w.runs_of("deploy").await.is_empty(),
        "deploy follows successes"
    );
    assert_eq!(w.runs_of("page").await.len(), 1);
}

#[tokio::test]
async fn a_fork_pull_request_starts_no_follow_up() {
    let w = World::new(&[(".scarab/ci.yaml", CI), (".scarab/deploy.yaml", DEPLOY)]);
    let pr = Event::PullRequest {
        actor: "stranger".into(),
        actor_id: String::new(),
        repo: repo(),
        number: 7,
        head: SHA.into(),
        title: "a change".into(),
        base: "main".into(),
        head_branch: "feature".into(),
        fork: true,
    };
    let runs = w.start(&pr).await;
    assert_eq!(runs.len(), 1);
    w.finish(&runs[0], ExecState::Succeeded).await;
    assert_eq!(w.drain().await, 0);
    assert!(w.runs_of("deploy").await.is_empty());
}

#[tokio::test]
async fn a_loop_of_follow_ups_is_refused() {
    // a follows b and b follows a: push → a → b → (a refused).
    const A: &str = r#"
on:
  push: {}
  upstream: { pipeline: b }
steps:
  - { id: one, image: busybox, command: ["true"] }
"#;
    const B: &str = r#"
on:
  upstream: { pipeline: a }
steps:
  - { id: two, image: busybox, command: ["true"] }
"#;
    let w = World::new(&[(".scarab/a.yaml", A), (".scarab/b.yaml", B)]);
    let runs = w.start(&push()).await;
    assert_eq!(runs.len(), 1);
    w.finish(&runs[0], ExecState::Succeeded).await;
    assert_eq!(w.drain().await, 1, "b follows a");
    let b = w.runs_of("b").await;
    assert_eq!(b.len(), 1);
    w.finish(&b[0].run, ExecState::Succeeded).await;
    assert_eq!(w.drain().await, 0, "a is already in the chain");
    assert_eq!(w.runs_of("a").await.len(), 1);
    let refused =
        w.db.refused_runs_for_tenant("acme", "app", 10)
            .await
            .unwrap();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].reason_kind, "upstream");
    assert!(
        refused[0].message.contains("a → b"),
        "the refusal names the chain: {}",
        refused[0].message
    );
}

#[tokio::test]
async fn a_rerun_that_finishes_again_starts_the_follow_up_again() {
    let w = World::new(&[(".scarab/ci.yaml", CI), (".scarab/deploy.yaml", DEPLOY)]);
    let ci = trigger_run_from_event(
        &w.forge,
        &w.db,
        &w.clock,
        AdmissionPorts::governance(None),
        &push(),
    )
    .await
    .unwrap()
    .pop()
    .unwrap();
    w.finish(&ci, ExecState::Succeeded).await;
    assert_eq!(w.drain().await, 1);
    scarab_engine::rerun_step(
        &w.db,
        &w.clock,
        &ci,
        &scarab_engine::StepId("build".into()),
        None,
    )
    .await
    .unwrap();
    w.finish(&ci, ExecState::Succeeded).await;
    assert_eq!(
        w.db.run_status(&ci).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    assert_eq!(w.drain().await, 1, "the Rerun's success is new news");
    assert_eq!(w.runs_of("deploy").await.len(), 2);
}

#[tokio::test]
async fn a_newer_follow_up_on_the_same_branch_supersedes_the_older() {
    // The follow-up keeps newest-wins: two `ci` Runs on main finish, and the
    // second one's deploy replaces the first one's while it is in flight.
    const TEST: &str = r#"
on:
  upstream: { pipeline: ci }
steps:
  - { id: slow, image: busybox, command: ["true"] }
"#;
    let w = World::new(&[(".scarab/test.yaml", TEST)]);
    let event = |run: &str| Event::Upstream {
        repo: repo(),
        run: run.into(),
        generation: 0,
        pipeline: "ci".into(),
        status: "succeeded".into(),
        r#ref: "refs/heads/main".into(),
        sha: SHA.into(),
        actor: String::new(),
        chain: vec!["ci".into()],
    };
    let older = w
        .start(&event("0190a1b2-c3d4-7e5f-8a9b-000000000001"))
        .await;
    w.clock.advance(1_000);
    let newer = w
        .start(&event("0190a1b2-c3d4-7e5f-8a9b-000000000002"))
        .await;
    assert_eq!((older.len(), newer.len()), (1, 1));
    assert_eq!(w.db.superseded_by(&newer[0]).await.unwrap(), older);
}

// ---------------------------------------------------------------------------
// A dispatch of fork code starts no follow-up (git-bug d910337)
// ---------------------------------------------------------------------------

/// `ci` as a dispatchable pipeline.
const CI_MANUAL: &str = r#"
on:
  manual: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

const OTHER: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";

impl World {
    /// Dispatch `ci` at `git_ref` by hand.
    async fn dispatch(&self, git_ref: &str) -> RunId {
        scarab_server::dispatch_run(
            &self.forge,
            &self.db,
            &self.clock,
            AdmissionPorts::governance(None),
            "alice".into(),
            repo(),
            git_ref.into(),
            "ci".into(),
            Default::default(),
            scarab_server::DispatchKind::Manual,
            None,
        )
        .await
        .expect("dispatch")
    }
}

/// A dispatch of a commit no branch or tag names is fork code: its Run
/// starts no follow-up, though the follow-up reads the same commit.
#[tokio::test]
async fn a_fork_code_dispatch_starts_no_follow_up() {
    let mut w = World::new(&[
        (".scarab/ci.yaml", CI_MANUAL),
        (".scarab/deploy.yaml", DEPLOY),
    ]);
    w.forge = std::mem::take(&mut w.forge).with_branch("main", OTHER);
    let run = w.dispatch(SHA).await;
    w.finish(&run, ExecState::Succeeded).await;
    assert_eq!(w.drain().await, 0);
    assert!(w.runs_of("deploy").await.is_empty());
}

/// A dispatch of the repository's own code starts its follow-up as before.
#[tokio::test]
async fn an_own_code_dispatch_starts_its_follow_up() {
    let mut w = World::new(&[
        (".scarab/ci.yaml", CI_MANUAL),
        (".scarab/deploy.yaml", DEPLOY),
    ]);
    w.forge = std::mem::take(&mut w.forge).with_branch("main", SHA);
    let run = w.dispatch("main").await;
    w.finish(&run, ExecState::Succeeded).await;
    assert_eq!(w.drain().await, 1);
    assert_eq!(w.runs_of("deploy").await.len(), 1);
}

/// Record `trigger` as Run `run`'s trigger.
async fn record_trigger(db: &InMemoryDb, run: &str, trigger: serde_json::Value) {
    use scarab_engine::{EventKind, EventPayload, Timestamp, EVENT_VERSION};
    let id = RunId(run.into());
    db.create_run(&id, 1, 1, Timestamp(0)).await.unwrap();
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: id,
        kind: EventPayload::Raw(serde_json::json!({ "trigger": { "event": trigger } })),
        at: Timestamp(0),
    })
    .await
    .unwrap();
}

/// What a recorded trigger says about fork code, failing closed: a dispatch
/// recorded before 0.8.4 has no `fork` and counts as fork code; a follow-up
/// is what the Run its chain started from is; a chain with a missing Run or
/// a cycle is fork code.
#[tokio::test]
async fn a_recorded_trigger_is_fork_code_unless_it_says_otherwise() {
    use scarab_server::trigger_is_fork_code;
    use serde_json::json;
    let db = InMemoryDb::new();
    let fork = |t: serde_json::Value| {
        let db = &db;
        async move { trigger_is_fork_code(db, &t).await.unwrap() }
    };

    assert!(
        fork(json!({ "kind": "manual", "sha": SHA })).await,
        "pre-0.8.4 dispatch"
    );
    assert!(
        fork(json!({ "kind": "api", "sha": SHA })).await,
        "pre-0.8.4 dispatch"
    );
    assert!(!fork(json!({ "kind": "manual", "fork": false })).await);
    assert!(fork(json!({ "kind": "api", "fork": true })).await);
    assert!(!fork(json!({ "kind": "push" })).await);
    assert!(fork(json!({ "kind": "pull_request", "fork": true })).await);

    record_trigger(&db, "own", json!({ "kind": "manual", "fork": false })).await;
    record_trigger(&db, "old", json!({ "kind": "manual" })).await;
    record_trigger(&db, "own-1", json!({ "kind": "upstream", "run": "own" })).await;
    record_trigger(&db, "old-1", json!({ "kind": "upstream", "run": "old" })).await;
    record_trigger(
        &db,
        "loop-a",
        json!({ "kind": "upstream", "run": "loop-b" }),
    )
    .await;
    record_trigger(
        &db,
        "loop-b",
        json!({ "kind": "upstream", "run": "loop-a" }),
    )
    .await;

    let upstream = |run: &str| json!({ "kind": "upstream", "run": run });
    assert!(!fork(upstream("own")).await, "a follow-up of own code");
    assert!(
        !fork(upstream("own-1")).await,
        "a follow-up of a follow-up of own code"
    );
    assert!(
        fork(upstream("old")).await,
        "a follow-up of a pre-0.8.4 dispatch"
    );
    assert!(
        fork(upstream("old-1")).await,
        "two links from a pre-0.8.4 dispatch"
    );
    assert!(
        fork(upstream("gone")).await,
        "an upstream Run that is not there"
    );
    assert!(
        fork(json!({ "kind": "upstream" })).await,
        "no upstream Run id"
    );
    assert!(fork(upstream("loop-a")).await, "a cycle");
}
