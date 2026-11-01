//! Lease terms fence the scheduler leader's writes (git-bug aaf4e3e,
//! ADR-0077 rank 4).
//!
//! A leader paused past its lease TTL (SIGSTOP, a GC pause, a VM suspend)
//! resumes in the middle of a leader-only leg. Before terms, nothing in the
//! leg's transactions re-checked the lease, so it wrote once more next to the
//! peer that had taken over. Each case here pauses the leader by construction
//! — a lease that expires while the test holds the leader still, then a peer
//! takeover — never by hoping a thread race lands.
//!
//! Runs on real Postgres (skips without `SCARAB_TEST_DATABASE_URL`) and on the
//! in-memory twin.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::leader::LeaderDb;
use scarab_engine::{
    Clock, Db, DbError, EventKind, EventPayload, LeaderTerm, RunId, RunStatus, Scheduler, StepId,
    StepStatus, Timestamp, EVENT_VERSION, SCHEDULER_LEASE,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

/// A lease TTL short enough to wait out in a test.
const TTL_MS: i64 = 300;

fn run_id() -> RunId {
    RunId("r1".into())
}

async fn term_of(db: &dyn Db, owner: &str) -> Option<LeaderTerm> {
    db.lease(SCHEDULER_LEASE, owner, TTL_MS)
        .await
        .unwrap()
        .held_by(SCHEDULER_LEASE, owner)
}

async fn wait_out_ttl() {
    tokio::time::sleep(Duration::from_millis(TTL_MS as u64 + 200)).await;
}

/// A run with one step already `Ready` (as admission leaves it).
async fn seed_ready(db: &dyn Db) {
    let at = Timestamp(1_000);
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &StepId("s".into()), None, &[], at)
        .await
        .unwrap();
    db.record_step_transition(
        &run_id(),
        &StepId("s".into()),
        StepStatus::Pending,
        StepStatus::Ready,
    )
    .await
    .unwrap();
}

fn note(at: i64) -> EventKind {
    EventKind {
        version: EVENT_VERSION,
        run: run_id(),
        kind: EventPayload::RunBudgetExhausted {
            active_ms: at,
            budget_ms: at,
        },
        at: Timestamp(at),
    }
}

/// A renewal keeps the term; a takeover by another owner increments it, and
/// so does taking it back.
async fn renewal_keeps_the_term_and_takeover_increments_it(db: &dyn Db) {
    let a1 = term_of(db, "a").await.expect("a takes the free lease");
    let renewed = term_of(db, "a").await.expect("a renews");
    assert_eq!(renewed, a1, "a renewal must not change the term");
    assert!(term_of(db, "b").await.is_none(), "b cannot take a live lease");

    wait_out_ttl().await;
    let b = term_of(db, "b").await.expect("b takes the expired lease");
    assert_eq!(b.term(), a1.term() + 1, "a takeover starts the next term");

    wait_out_ttl().await;
    let a2 = term_of(db, "a").await.expect("a takes it back");
    assert_eq!(a2.term(), a1.term() + 2);
    assert_ne!(a2, a1, "the same owner on a later term is a different fence");
}

/// A leads; its lease expires; B takes over. A's leader-only writes on its
/// old term are refused and change nothing, both the Db methods that take the
/// term and the shared writes made through the leader's view. B's succeed.
async fn deposed_leader_writes_are_refused(db: &dyn Db) {
    seed_ready(db).await;
    let a = term_of(db, "a").await.unwrap();
    wait_out_ttl().await; // A is "paused" here
    let b = term_of(db, "b").await.expect("b takes over");

    // A Db method only a leader calls: refused, the step stays Ready.
    assert!(matches!(
        db.claim_ready_steps(&a, 10).await,
        Err(DbError::NotLeader)
    ));
    assert!(matches!(
        db.acquire_slot(&a, "group", &run_id()).await,
        Err(DbError::NotLeader)
    ));
    // A shared write through A's leader view: refused, nothing appended.
    let events_before = db.events(&run_id()).await.unwrap().len();
    assert!(matches!(
        LeaderDb::new(db, a.clone()).append_event(&note(1)).await,
        Err(DbError::NotLeader)
    ));
    let steps = db.steps_of_run(&run_id()).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Ready, "A's claim changed nothing");
    assert_eq!(db.events(&run_id()).await.unwrap().len(), events_before);
    assert_eq!(db.run_concurrency(&run_id()).await.unwrap(), None);

    // B's writes go through.
    let claimed = db.claim_ready_steps(&b, 10).await.unwrap();
    assert_eq!(claimed.len(), 1);
    LeaderDb::new(db, b).append_event(&note(2)).await.unwrap();
    assert_eq!(db.events(&run_id()).await.unwrap().len(), events_before + 1);
}

/// While a leader's fenced write is in flight, a peer cannot take the lease
/// over even though it has expired: the takeover is refused (postgres: its
/// lock wait times out) and reports the holder. After the write, it succeeds.
async fn takeover_waits_for_an_inflight_fenced_write(db: &dyn Db) {
    seed_ready(db).await;
    let a = term_of(db, "a").await.unwrap();
    wait_out_ttl().await;
    let seen = Arc::new(Mutex::new(None));
    let seen_in = seen.clone();
    db.fenced_write(
        &a,
        Box::pin(async move {
            let lease = db.lease(SCHEDULER_LEASE, "b", TTL_MS).await?;
            *seen_in.lock().unwrap() = Some(lease.owner);
            db.append_event(&note(3)).await
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        seen.lock().unwrap().as_deref(),
        Some("a"),
        "b took the lease over in the middle of a's fenced write"
    );
    let b = term_of(db, "b").await.expect("b takes over once the write ends");
    assert_eq!(b.term(), a.term() + 1);
}

/// A clock whose first `now()` runs a one-shot hook: the scheduler reads the
/// clock just before its first write in `admit`, so the hook is a leader
/// paused between deciding and writing.
struct PausingClock {
    inner: FakeClock,
    hook: Mutex<Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>>,
}

#[async_trait]
impl Clock for PausingClock {
    async fn now(&self) -> Timestamp {
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook.await;
        }
        self.inner.now().await
    }
    async fn sleep(&self, ms: u64) {
        self.inner.sleep(ms).await
    }
}

/// End to end through the scheduler: A starts admitting a Pending run, is
/// paused past its TTL and deposed before its first write, then resumes. The
/// write is refused, `admit` returns quietly (no error for the run's failure
/// streak), and the run is left for B, who admits it.
async fn scheduler_deposed_mid_admit_writes_nothing(db: Arc<dyn Db>) {
    seed_ready(&*db).await;
    let db_hook = db.clone();
    let clock = PausingClock {
        inner: FakeClock::new(1_000),
        hook: Mutex::new(Some(Box::pin(async move {
            wait_out_ttl().await;
            assert!(term_of(&*db_hook, "b").await.is_some(), "b takes over");
        }))),
    };
    let exec = FakeExecutor::new();
    Scheduler::new(&*db, &clock, &exec, "a")
        .with_lease_ttl_ms(TTL_MS)
        .admit(&run_id())
        .await
        .expect("a deposed leader's admit ends quietly");
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Pending),
        "the deposed leader started the run"
    );
    assert!(db.events(&run_id()).await.unwrap().is_empty());

    Scheduler::new(&*db, &clock, &exec, "b")
        .with_lease_ttl_ms(TTL_MS)
        .admit(&run_id())
        .await
        .unwrap();
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Running)
    );
}

async fn pg() -> Option<(Arc<PostgresDb>, common::TestDb)> {
    let tdb = fresh_db().await?;
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    Some((Arc::new(db), tdb))
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_renewal_keeps_the_term_and_takeover_increments_it() {
    let Some((db, tdb)) = pg().await else { return };
    renewal_keeps_the_term_and_takeover_increments_it(&*db).await;
    tdb.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_deposed_leader_writes_are_refused() {
    let Some((db, tdb)) = pg().await else { return };
    deposed_leader_writes_are_refused(&*db).await;
    tdb.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_takeover_waits_for_an_inflight_fenced_write() {
    let Some((db, tdb)) = pg().await else { return };
    takeover_waits_for_an_inflight_fenced_write(&*db).await;
    tdb.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_scheduler_deposed_mid_admit_writes_nothing() {
    let Some((db, tdb)) = pg().await else { return };
    scheduler_deposed_mid_admit_writes_nothing(db).await;
    tdb.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn inmemory_renewal_keeps_the_term_and_takeover_increments_it() {
    renewal_keeps_the_term_and_takeover_increments_it(&InMemoryDb::new()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn inmemory_deposed_leader_writes_are_refused() {
    deposed_leader_writes_are_refused(&InMemoryDb::new()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn inmemory_takeover_waits_for_an_inflight_fenced_write() {
    takeover_waits_for_an_inflight_fenced_write(&InMemoryDb::new()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn inmemory_scheduler_deposed_mid_admit_writes_nothing() {
    scheduler_deposed_mid_admit_writes_nothing(Arc::new(InMemoryDb::new())).await;
}
