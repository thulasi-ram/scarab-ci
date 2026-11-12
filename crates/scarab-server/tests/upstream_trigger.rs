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
