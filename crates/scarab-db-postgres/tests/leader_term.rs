//! Lease terms fence the scheduler leader's writes (git-bug aaf4e3e,
//! ADR-0077 rank 4).
//!
//! A leader paused past its lease TTL (SIGSTOP, a GC pause, a VM suspend)
//! resumes in the middle of a leader-only leg. Every write that a deposed
//! leader could not safely commit late checks owner and term inside its OWN
//! transaction (`SELECT … FROM leases … FOR SHARE`), so it is refused or
//! committed before the takeover. Each case pauses or blocks the leader by
//! construction — a lease that expires while the test holds the leader still,
//! a row lock a test transaction holds, a future the test stops polling —
//! never by hoping a thread race lands.
//!
//! Runs on real Postgres (skips without `SCARAB_TEST_DATABASE_URL`) and, where
//! the case is not about Postgres locks, on the in-memory twin.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Clock, Db, DbError, EventKind, EventPayload, LeaderTerm,
    RunId, RunStatus, Scheduler, ServiceStatus, StepId, StepLaunch, StepRun, StepSpec, StepStatus,
    Timestamp, EVENT_VERSION, SCHEDULER_LEASE,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

/// A lease TTL short enough to wait out in a test.
const TTL_MS: i64 = 300;
/// The leader-fenced transaction bound the Postgres cases run with.
const BOUND: Duration = Duration::from_millis(1_000);

fn run_id() -> RunId {
    RunId("r1".into())
}

fn step_id() -> StepId {
    StepId("s".into())
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
    db.create_step_run(&run_id(), &step_id(), None, &[], at)
        .await
        .unwrap();
    db.record_step_transition(
        &run_id(),
        &step_id(),
        StepStatus::Pending,
        StepStatus::Ready,
    )
    .await
    .unwrap();
}

/// A claimed step's launch: its next Attempt and one event.
fn launch(s: &StepRun) -> Result<StepLaunch, DbError> {
    let id = AttemptId(format!("a{}", s.attempts.len() + 1));
    Ok(StepLaunch {
        attempt: Attempt {
            id: id.clone(),
            started_at: Timestamp(1_000),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        },
        events: vec![EventKind {
            version: EVENT_VERSION,
            run: s.run.clone(),
            kind: EventPayload::AttemptStarted {
                step: s.step.clone(),
                attempt: id,
            },
            at: Timestamp(1_000),
        }],
        outbox: Vec::new(),
    })
}

/// `leader`'s admission of the seeded run: `Pending → Running`, fenced.
async fn admit_run(db: &dyn Db, leader: &LeaderTerm) -> Result<(), DbError> {
    db.commit_run_transition(
        Some(leader),
        &run_id(),
        RunStatus::Pending,
        RunStatus::Running,
        0,
        &[],
        &[],
    )
    .await
}

/// A renewal keeps the term; a takeover by another owner increments it, and
/// so does taking it back.
async fn renewal_keeps_the_term_and_takeover_increments_it(db: &dyn Db) {
    let a1 = term_of(db, "a").await.expect("a takes the free lease");
    let renewed = term_of(db, "a").await.expect("a renews");
    assert_eq!(renewed, a1, "a renewal must not change the term");
    assert!(
        term_of(db, "b").await.is_none(),
        "b cannot take a live lease"
    );

    wait_out_ttl().await;
    let b = term_of(db, "b").await.expect("b takes the expired lease");
    assert_eq!(b.term(), a1.term() + 1, "a takeover starts the next term");

    wait_out_ttl().await;
    let a2 = term_of(db, "a").await.expect("a takes it back");
    assert_eq!(a2.term(), a1.term() + 2);
    assert_ne!(
        a2, a1,
        "the same owner on a later term is a different fence"
    );
}

/// A leads; its lease expires; B takes over. Every fenced write on A's old
/// term is refused and changes nothing; B's succeed.
async fn deposed_leader_writes_are_refused(db: &dyn Db) {
    seed_ready(db).await;
    db.create_step_run(&run_id(), &StepId("p".into()), None, &[], Timestamp(1_000))
        .await
        .unwrap();
    db.create_run_service(None, &run_id(), 1, "pg", Timestamp(1_000))
        .await
        .unwrap();
    let a = term_of(db, "a").await.unwrap();
    wait_out_ttl().await; // A is "paused" here
    let b = term_of(db, "b").await.expect("b takes over");

    let refused = |r: Result<(), DbError>, what: &str| {
        assert!(matches!(r, Err(DbError::NotLeader)), "{what}: {r:?}");
    };
    refused(
        db.claim_ready_steps(&a, 10, &launch).await.map(|_| ()),
        "claim",
    );
    refused(
        db.acquire_slot(&a, "group", &run_id()).await.map(|_| ()),
        "acquire_slot",
    );
    refused(
        db.promote_step(&a, &run_id(), &StepId("p".into()), "sig", &[])
            .await,
        "promote_step",
    );
    refused(admit_run(db, &a).await, "commit_run_transition");
    refused(
        db.set_run_service(Some(&a), &run_id(), 1, "pg", ServiceStatus::Failed, None)
            .await,
        "set_run_service",
    );
    refused(
        db.create_run_service(Some(&a), &run_id(), 2, "pg", Timestamp(1_000))
            .await,
        "create_run_service",
    );
    let steps = db.steps_of_run(&run_id()).await.unwrap();
    let status = |id: &str| steps.iter().find(|s| s.step.0 == id).unwrap().status;
    assert_eq!(status("s"), StepStatus::Ready, "A's claim changed nothing");
    assert_eq!(
        status("p"),
        StepStatus::Pending,
        "A's promotion changed nothing"
    );
    assert!(db.events(&run_id()).await.unwrap().is_empty());
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Pending)
    );
    let services = db.run_services(&run_id()).await.unwrap();
    assert_eq!(services.len(), 1, "{services:?}");
    assert_eq!(services[0].status, ServiceStatus::Starting);

    // B's writes go through.
    assert_eq!(
        db.claim_ready_steps(&b, 10, &launch).await.unwrap().len(),
        1
    );
    db.promote_step(&b, &run_id(), &StepId("p".into()), "sig", &[])
        .await
        .unwrap();
    admit_run(db, &b).await.unwrap();
    db.set_run_service(Some(&b), &run_id(), 1, "pg", ServiceStatus::Ready, None)
        .await
        .unwrap();
    let s = db.steps_of_run(&run_id()).await.unwrap();
    let claimed = s.iter().find(|s| s.step.0 == "s").unwrap();
    assert_eq!(claimed.status, StepStatus::Running);
    assert_eq!(claimed.attempts.len(), 1, "the claim minted its Attempt");
    assert_eq!(db.events(&run_id()).await.unwrap().len(), 1);
    assert_eq!(
        db.step_input(&run_id(), &StepId("p".into())).await.unwrap(),
        Some("sig".into())
    );
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

// --- Postgres: row locks, stalls and bounds ---------------------------------

async fn pg_bounded() -> Option<(Arc<PostgresDb>, common::TestDb)> {
    let tdb = fresh_db().await?;
    let db = PostgresDb::with_pool(tdb.pool.clone()).with_leader_tx_bound(BOUND);
    db.migrate().await.unwrap();
    Some((Arc::new(db), tdb))
}

/// Some other transaction holds the seeded run's row (an API cancel, a
/// peer's commit, a peer paused mid-transaction).
async fn lock_run_row(pool: &sqlx::PgPool) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM runs WHERE id = 'r1' FOR UPDATE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    blocker
}

/// Wait until some backend is blocked on a lock running `UPDATE runs`.
async fn wait_for_run_update_blocked(pool: &sqlx::PgPool) {
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE wait_event_type = 'Lock' AND query LIKE 'UPDATE runs%'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if n > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the fenced UPDATE never blocked on the row lock");
}

/// B tries to take the lease over until it does; panics past `within`.
async fn takeover_by_b(db: &dyn Db, within: Duration) -> (LeaderTerm, Duration) {
    let t0 = Instant::now();
    loop {
        if let Some(b) = term_of(db, "b").await {
            return (b, t0.elapsed());
        }
        assert!(
            t0.elapsed() < within,
            "no peer took the lease over within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// While the leader's fenced write is in flight, a takeover of its expired
/// lease waits for it: the write commits first, and the takeover lands after
/// it, never between the term check and the commit.
#[tokio::test(flavor = "multi_thread")]
async fn pg_a_takeover_is_ordered_after_an_inflight_fenced_write() {
    let Some(tdb) = fresh_db().await else { return };
    // A bound well past the takeover's 1 s wait, so A's write is still in
    // flight when B gives up.
    let db = Arc::new(
        PostgresDb::with_pool(tdb.pool.clone()).with_leader_tx_bound(Duration::from_secs(10)),
    );
    db.migrate().await.unwrap();
    seed_ready(&*db).await;
    let a = term_of(&*db, "a").await.unwrap();
    wait_out_ttl().await;
    let blocker = lock_run_row(&tdb.pool).await;
    let write = {
        let (db, a) = (db.clone(), a.clone());
        tokio::spawn(async move { admit_run(&*db, &a).await })
    };
    wait_for_run_update_blocked(&tdb.pool).await;
    // A's write holds the lease row FOR SHARE: B's takeover gives up after
    // its short wait and reports A.
    let seen = db.lease(SCHEDULER_LEASE, "b", TTL_MS).await.unwrap();
    assert_eq!(
        seen.owner, "a",
        "b took over in the middle of a's fenced write"
    );
    blocker.rollback().await.unwrap();
    write
        .await
        .unwrap()
        .expect("a's write commits: a still held its term");
    let (b, _) = takeover_by_b(&*db, Duration::from_secs(5)).await;
    assert_eq!(b.term(), a.term() + 1);
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Running)
    );
    tdb.cleanup().await;
}

/// Red team A1 (review of the fence-pool design): a deposed leader's write
/// that is blocked on a row lock must not commit after the takeover.
///
/// A's fenced write is blocked on the run row (held by `blocker`), and A is
/// then stopped: its future is not polled. Postgres cancels the blocked
/// statement at the bound and ends the stopped session, which releases the
/// lease row; B takes over on the next term. The blocker lets go, and A's
/// write — which, when the fence ran on a separate connection from the
/// write, then autocommitted while A was still stopped — never lands.
#[tokio::test(flavor = "multi_thread")]
async fn pg_a_stopped_leaders_blocked_write_never_commits_after_the_takeover() {
    let Some((db, tdb)) = pg_bounded().await else {
        return;
    };
    seed_ready(&*db).await;
    let a = term_of(&*db, "a").await.unwrap();
    let blocker = lock_run_row(&tdb.pool).await;

    let mut stopped = Box::pin(admit_run(&*db, &a));
    tokio::select! {
        r = &mut stopped => panic!("A's write cannot finish while the row is locked: {r:?}"),
        _ = wait_for_run_update_blocked(&tdb.pool) => {}
    }
    // A is stopped from here on: `stopped` is not polled.

    let within = Duration::from_millis(TTL_MS as u64) + BOUND * 2 + Duration::from_secs(3);
    let (b, took) = takeover_by_b(&*db, within).await;
    assert_eq!(b.term(), a.term() + 1, "B leads on the next term");
    eprintln!("B took over {took:?} after A was stopped inside its fenced write");

    // The row lock goes away while A is still stopped.
    blocker.rollback().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Pending),
        "deposed leader A (term {}) started the run after B took over on term {}",
        a.term(),
        b.term()
    );
    let resumed = tokio::time::timeout(Duration::from_secs(10), stopped)
        .await
        .expect("A's write ends when A resumes");
    assert!(resumed.is_err(), "A was told its write landed: {resumed:?}");
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Pending)
    );
    assert!(matches!(admit_run(&*db, &a).await, Err(DbError::NotLeader)));
    admit_run(&*db, &b).await.unwrap();
    tdb.cleanup().await;
}

/// Red team A2: a LIVE leader whose fenced write waits on a row lock must
/// not freeze leadership for as long as the lock is held.
///
/// A keeps polling its write the whole time. Its transaction gives up at the
/// bound ([`DbError::FenceTimeout`], nothing written) and releases the lease
/// row, so B — finding A's lease expired — takes over within TTL + bound,
/// while the row lock is still held. (With the fence held by a keepalive on a
/// separate connection, every takeover hit its lock timeout and reported A
/// for as long as the row stayed locked.)
#[tokio::test(flavor = "multi_thread")]
async fn pg_a_live_leader_blocked_in_a_fenced_write_does_not_freeze_leadership() {
    let Some((db, tdb)) = pg_bounded().await else {
        return;
    };
    seed_ready(&*db).await;
    let a = term_of(&*db, "a").await.unwrap();
    let blocker = lock_run_row(&tdb.pool).await;

    let window = Duration::from_millis(TTL_MS as u64) + BOUND * 6;
    let (written, took) = tokio::join!(admit_run(&*db, &a), async {
        let t0 = Instant::now();
        while t0.elapsed() < window {
            let l = db.lease(SCHEDULER_LEASE, "b", TTL_MS).await.unwrap();
            if l.owner == "b" {
                return Some(t0.elapsed());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    });
    let took = took.unwrap_or_else(|| {
        panic!(
            "for {window:?} (TTL {TTL_MS} ms + 6x the {BOUND:?} bound) no peer could take the \
             lease over from a leader blocked inside one fenced write"
        )
    });
    eprintln!("B took over {took:?} into A's blocked write; A was told {written:?}");
    assert!(
        matches!(written, Err(DbError::FenceTimeout)),
        "A's blocked write must time out with nothing written: {written:?}"
    );
    blocker.rollback().await.unwrap();
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Pending)
    );
    tdb.cleanup().await;
}

/// A leader stopped BETWEEN two statements of a fenced transaction (idle in
/// it, holding the lease row) loses the lease within TTL + bound: Postgres
/// ends the idle session and rolls the transaction back. Resumed, its claim
/// fails and claimed nothing — the step is still `Ready` with no Attempt for
/// B to claim. A is its own process here: a thread with its own runtime and
/// pool, whose claim's `mint` blocks that thread until the test has seen B
/// take over.
#[tokio::test(flavor = "multi_thread")]
async fn pg_a_leader_stopped_idle_inside_its_claim_loses_the_lease_and_claims_nothing() {
    let Some((db, tdb)) = pg_bounded().await else {
        return;
    };
    seed_ready(&*db).await;
    let a = term_of(&*db, "a").await.unwrap();
    let (stopped_tx, stopped) = std::sync::mpsc::channel::<()>();
    let (resume, resume_rx) = std::sync::mpsc::channel::<()>();
    let resume_rx = Mutex::new(resume_rx);
    let opts = (*tdb.pool.connect_options()).clone();
    let a_process = {
        let a = a.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(2)
                    .connect_with(opts)
                    .await
                    .unwrap();
                let db_a = PostgresDb::with_pool(pool).with_leader_tx_bound(BOUND);
                let mint = move |s: &StepRun| {
                    let _ = stopped_tx.send(());
                    // Stopped: the whole process waits here, mid-transaction.
                    let _ = resume_rx.lock().unwrap().recv();
                    launch(s)
                };
                db_a.claim_ready_steps(&a, 10, &mint)
                    .await
                    .map(|claimed| claimed.len())
            })
        })
    };
    tokio::task::spawn_blocking(move || stopped.recv().unwrap())
        .await
        .unwrap();

    let within = Duration::from_millis(TTL_MS as u64) + BOUND + Duration::from_secs(4);
    let (b, took) = takeover_by_b(&*db, within).await;
    assert_eq!(b.term(), a.term() + 1);
    eprintln!("B took over {took:?} after A stopped idle inside its claim");
    resume.send(()).unwrap();
    let resumed = tokio::task::spawn_blocking(move || a_process.join().unwrap())
        .await
        .unwrap();
    eprintln!("A resumed and was told {resumed:?}");
    assert!(resumed.is_err(), "A's claim reported success: {resumed:?}");
    let steps = db.steps_of_run(&run_id()).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Ready, "A's claim landed");
    assert!(steps[0].attempts.is_empty());
    assert_eq!(
        db.claim_ready_steps(&b, 10, &launch).await.unwrap().len(),
        1
    );
    tdb.cleanup().await;
}

/// A follower's `lease()` does not wait for the leader's fenced transaction:
/// it reads the live lease without a lock. (When it took the row lock to
/// read it, every follower `lease()` — twice per active run per tick — waited
/// up to its 2 s lock timeout while the leader was inside a fenced write.)
#[tokio::test(flavor = "multi_thread")]
async fn pg_a_followers_lease_does_not_wait_for_the_leaders_fence() {
    let Some((db, tdb)) = pg_bounded().await else {
        return;
    };
    db.lease(SCHEDULER_LEASE, "a", 60_000).await.unwrap();
    // The leader's in-flight fenced transaction, as it holds the lease row.
    let mut fence = tdb.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM leases WHERE resource = 'scheduler' FOR SHARE")
        .execute(&mut *fence)
        .await
        .unwrap();
    let mut slowest = Duration::ZERO;
    for _ in 0..20 {
        let t0 = Instant::now();
        let seen = db.lease(SCHEDULER_LEASE, "b", 60_000).await.unwrap();
        slowest = slowest.max(t0.elapsed());
        assert_eq!(seen.owner, "a");
    }
    eprintln!("slowest follower lease() under the leader's fence: {slowest:?}");
    assert!(
        slowest < Duration::from_millis(500),
        "a follower's lease() waited {slowest:?} on the leader's fence"
    );
    fence.rollback().await.unwrap();
    tdb.cleanup().await;
}

/// A fenced transaction that times out is no failure of the run: with a
/// zero dead-letter deadline (any isolated failure dead-letters at once),
/// a tick whose admission hit the bound reports nothing and dead-letters
/// nothing, and the next tick admits the run.
#[tokio::test(flavor = "multi_thread")]
async fn pg_a_timed_out_fenced_write_defers_the_leg_quietly() {
    let Some((db, tdb)) = pg_bounded().await else {
        return;
    };
    seed_ready(&*db).await;
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&*db, &clock, &exec, "a").with_tick_failure_deadline_ms(0);
    let blocker = lock_run_row(&tdb.pool).await;
    let isolated = sched.tick_all().await.unwrap();
    assert!(isolated.is_empty(), "{isolated:?}");
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Pending),
        "the timed-out admission dead-lettered or started the run"
    );
    blocker.rollback().await.unwrap();
    let isolated = sched.tick_all().await.unwrap();
    assert!(isolated.is_empty(), "{isolated:?}");
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Running)
    );
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
async fn inmemory_scheduler_deposed_mid_admit_writes_nothing() {
    scheduler_deposed_mid_admit_writes_nothing(Arc::new(InMemoryDb::new())).await;
}

/// A's clock, which deposes A (waits out its TTL, then lets B take the lease
/// over) the first time A reads it while one of `run`'s steps is `Running`:
/// that is A paused right after its claim committed (git-bug 0f29112).
struct DeposeAfterClaim {
    inner: FakeClock,
    db: Arc<dyn Db>,
    fired: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Clock for DeposeAfterClaim {
    async fn now(&self) -> Timestamp {
        use std::sync::atomic::Ordering;
        if !self.fired.load(Ordering::SeqCst) {
            let steps = self.db.steps_of_run(&run_id()).await.unwrap();
            if steps.iter().any(|s| s.status == StepStatus::Running) {
                self.fired.store(true, Ordering::SeqCst);
                wait_out_ttl().await;
                assert!(term_of(&*self.db, "b").await.is_some(), "b takes over");
            }
        }
        self.inner.now().await
    }
    async fn sleep(&self, ms: u64) {
        self.inner.sleep(ms).await
    }
}

/// A step the fake executor can launch.
fn spec() -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["true".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        forge_token: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
        scope: None,
    }
}

/// git-bug 0f29112: A claims a Run's ready steps and is deposed right after
/// the claim commits. Every claimed step must already carry its Attempt and
/// launch intent, so B finishes the Run. (When the claim and the Attempt were
/// separate writes, A's Attempt write was refused and the steps stayed
/// `Running` with no Attempt: B claims only `Ready` steps, so the Run hung.)
async fn a_leader_deposed_after_its_claim_leaves_a_run_its_successor_finishes(db: Arc<dyn Db>) {
    let at = Timestamp(1_000);
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    for s in ["s1", "s2"] {
        db.create_step_run(&run_id(), &StepId(s.into()), Some(&spec()), &[], at)
            .await
            .unwrap();
    }
    let exec = FakeExecutor::new();
    for _ in 0..8 {
        exec.script_outcome(scarab_engine::ports::ExecState::Succeeded);
    }
    let clock_a = Arc::new(DeposeAfterClaim {
        inner: FakeClock::new(1_000),
        db: db.clone(),
        fired: Default::default(),
    });
    Scheduler::new(&*db, &*clock_a, &exec, "a")
        .with_lease_ttl_ms(TTL_MS)
        .tick_all()
        .await
        .unwrap();
    // A reads no clock between the claim and its launches any more (they are
    // one commit); then A is deposed right after its tick instead — still
    // after the claim committed.
    if !clock_a
        .fired
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        wait_out_ttl().await;
        assert!(term_of(&*db, "b").await.is_some(), "b takes over");
    }
    for s in db.steps_of_run(&run_id()).await.unwrap() {
        if s.status == StepStatus::Running {
            assert!(
                !s.attempts.is_empty(),
                "{} is Running with no Attempt after A was deposed",
                s.step.0
            );
        }
    }

    let clock_b = FakeClock::new(2_000);
    let b = Scheduler::new(&*db, &clock_b, &exec, "b").with_lease_ttl_ms(600_000);
    for _ in 0..20 {
        b.tick_all().await.unwrap();
        if db.run_status(&run_id()).await.unwrap() == Some(RunStatus::Succeeded) {
            return;
        }
    }
    panic!(
        "B never finished the Run A was deposed from: {:?}",
        db.steps_of_run(&run_id())
            .await
            .unwrap()
            .iter()
            .map(|s| (s.step.0.clone(), s.status, s.attempts.len()))
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_a_leader_deposed_after_its_claim_leaves_a_run_its_successor_finishes() {
    let Some((db, tdb)) = pg().await else { return };
    a_leader_deposed_after_its_claim_leaves_a_run_its_successor_finishes(db).await;
    tdb.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn inmemory_a_leader_deposed_after_its_claim_leaves_a_run_its_successor_finishes() {
    a_leader_deposed_after_its_claim_leaves_a_run_its_successor_finishes(Arc::new(
        InMemoryDb::new(),
    ))
    .await;
}
