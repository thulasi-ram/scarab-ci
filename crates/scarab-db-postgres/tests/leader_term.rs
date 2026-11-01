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

/// A leader stopped INSIDE a fenced write (a SIGSTOP, a GC pause) must not
/// hold the lease row until its connection dies: Postgres ends the stalled
/// fence after the stall bound and a peer takes the lease over within
/// TTL + bound. The stopped leader is modelled exactly: its fenced write is
/// polled until the fence is held, then not polled at all — no keepalive, no
/// progress, its connections open. Resumed, its fence is gone: the write
/// reports NotLeader, and its old term is refused from then on.
#[tokio::test(flavor = "multi_thread")]
async fn pg_a_leader_stopped_inside_a_fenced_write_loses_the_lease() {
    let Some(tdb) = fresh_db().await else { return };
    let bound = Duration::from_millis(1_000);
    let db = PostgresDb::with_pool(tdb.pool.clone()).with_fence_stall_bound(bound);
    db.migrate().await.unwrap();
    seed_ready(&db).await;
    let a = term_of(&db, "a").await.unwrap();

    let (held, fence_held) = tokio::sync::oneshot::channel::<()>();
    let mut stopped = Box::pin(db.fenced_write(
        &a,
        Box::pin(async move {
            let _ = held.send(());
            std::future::pending::<Result<(), DbError>>().await
        }),
    ));
    tokio::select! {
        r = &mut stopped => panic!("the fenced write cannot finish: {r:?}"),
        _ = fence_held => {}
    }
    // `stopped` is not polled from here on: the leader is stopped.

    let t0 = std::time::Instant::now();
    let deadline = Duration::from_millis(TTL_MS as u64) + bound + Duration::from_secs(4);
    let b = loop {
        if let Some(b) = term_of(&db, "b").await {
            break b;
        }
        assert!(
            t0.elapsed() < deadline,
            "no peer took the lease over within TTL + stall bound: the stopped \
             leader's fence still holds the lease row"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(b.term(), a.term() + 1);

    // The leader resumes: its fence is gone.
    let resumed = tokio::time::timeout(Duration::from_secs(10), stopped)
        .await
        .expect("the resumed fenced write ends");
    assert!(matches!(resumed, Err(DbError::NotLeader)), "{resumed:?}");
    assert!(matches!(
        LeaderDb::new(&db, a).append_event(&note(4)).await,
        Err(DbError::NotLeader)
    ));
    LeaderDb::new(&db, b).append_event(&note(5)).await.unwrap();
    tdb.cleanup().await;
}

/// As many concurrent fenced writes as the main pool has connections, each
/// holding its fence until all are held, complete: the fence connections come
/// from their own pool, so a fence never waits for a connection another fence
/// is holding. From one shared pool every connection would hold a fence and
/// every write would wait for one more.
#[tokio::test(flavor = "multi_thread")]
async fn pg_pool_size_concurrent_fenced_writes_complete() {
    let Some(tdb) = fresh_db().await else { return };
    const POOL: u32 = 3;
    let small = sqlx::postgres::PgPoolOptions::new()
        .max_connections(POOL)
        .acquire_timeout(Duration::from_secs(60))
        .connect_with((*tdb.pool.connect_options()).clone())
        .await
        .unwrap();
    let db = Arc::new(PostgresDb::with_pool(small));
    db.migrate().await.unwrap();
    seed_ready(&*db).await;
    let a = term_of(&*db, "a").await.unwrap();
    let all_held = Arc::new(tokio::sync::Barrier::new(POOL as usize));
    let mut writes = tokio::task::JoinSet::new();
    for i in 0..POOL {
        let (db, a, all_held) = (db.clone(), a.clone(), all_held.clone());
        writes.spawn(async move {
            let db2 = db.clone();
            db.fenced_write(
                &a,
                Box::pin(async move {
                    all_held.wait().await;
                    db2.append_event(&note(10 + i as i64)).await
                }),
            )
            .await
        });
    }
    let done = tokio::time::timeout(Duration::from_secs(20), writes.join_all())
        .await
        .expect("pool-size concurrent fenced writes deadlocked");
    assert!(done.iter().all(Result::is_ok), "{done:?}");
    assert_eq!(db.events(&run_id()).await.unwrap().len(), POOL as usize);
    tdb.cleanup().await;
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
