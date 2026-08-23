//! Multi-replica log tailing (ADR-0051), hermetic with two simulated
//! replicas: the claim-to-tail lease lets exactly ONE replica ingest a step's
//! log (no duplicate chunks with 2 tailers), and the live SSE path serves
//! from the durable index — so the replica that did NOT tail still streams
//! the log live.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Db, ExecError, Executor, LogChunks, RunId, StepId, StepRun,
    StepStatus, Timestamp,
};
use scarab_server::log_tail::LogTailer;
use scarab_server::LogService;
use scarab_testkit::{InMemoryDb, InMemoryObjectStore};

/// An executor whose log stream yields two fixed chunks, counting how many
/// streams were ever opened (the duplicate-ingestion detector).
#[derive(Default)]
struct TwoChunkExec {
    opened: AtomicU32,
}

struct TwoChunks {
    left: Mutex<Vec<Vec<u8>>>,
}

#[async_trait]
impl LogChunks for TwoChunks {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ExecError> {
        Ok(self.left.lock().unwrap().pop())
    }
}

#[async_trait]
impl Executor for TwoChunkExec {
    async fn launch(
        &self,
        _s: &StepRun,
        _spec: &scarab_engine::StepSpec,
    ) -> Result<ExecHandle, ExecError> {
        Ok(ExecHandle("h".into()))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        Ok(ExecState::Running)
    }
    async fn cancel(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
    // ADR-0064 s2: required (never defaulted) so the compiler makes every impl —
    // wrappers included — decide; this stub snapshots nothing, so no stamp.
    async fn log_stream(&self, _s: &StepRun) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Some(Box::new(TwoChunks {
            left: Mutex::new(vec![b"line-2\n".to_vec(), b"line-1\n".to_vec()]),
        })))
    }
}

fn running_step() -> StepRun {
    StepRun {
        run: RunId("r1".into()),
        step: StepId("s1".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    }
}

#[tokio::test]
async fn two_replicas_tail_a_step_exactly_once() {
    // ONE durable world (db + store), TWO replica-local tailers.
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let exec = Arc::new(TwoChunkExec::default());

    let logs_a = Arc::new(LogService::new(store.clone(), db.clone()));
    let logs_b = Arc::new(LogService::new(store.clone(), db.clone()));
    let tailer_a = LogTailer::new(exec.clone(), logs_a).with_lease(db.clone(), "replica-a");
    let tailer_b = LogTailer::new(exec.clone(), logs_b).with_lease(db.clone(), "replica-b");

    // Both replicas' driver ticks see the same running step.
    let step = running_step();
    tailer_a.ensure(&step);
    tailer_b.ensure(&step);

    // Let both claim attempts + drains settle.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Exactly one stream was opened, and the durable index holds the two
    // chunks exactly once — no duplicate ingestion.
    assert_eq!(
        exec.opened.load(Ordering::SeqCst),
        1,
        "one tailer won the lease"
    );
    let chunks = db
        .log_chunks(
            &RunId("r1".into()),
            &StepId("s1".into()),
            &AttemptId("a1".into()),
        )
        .await
        .unwrap();
    assert_eq!(chunks.len(), 2, "two chunks, ingested once: {chunks:?}");
}

#[tokio::test]
async fn sse_live_tail_is_replica_agnostic() {
    use axum::body::Body;
    use axum::http::Request;
    use futures::StreamExt;
    use scarab_server::{router, AppState};
    use scarab_testkit::FakeClock;
    use tower::ServiceExt;

    // Replica A ingests; replica B serves the SSE. They share ONLY the
    // durable world (db + object store) — no in-process broadcast crosses.
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs_a = Arc::new(LogService::new(store.clone(), db.clone()));
    let logs_b = Arc::new(LogService::new(store.clone(), db.clone()));

    // A running run with one launched step.
    let run = RunId("r-sse".into());
    db.seed_run(&run, scarab_engine::RunStatus::Running);
    db.seed_ready(vec![running_step_with_run("r-sse")]);

    // Replica A commits a chunk BEFORE the SSE request (the replay part)…
    logs_a
        .append(
            &run,
            &StepId("s1".into()),
            &AttemptId("a1".into()),
            b"early\n",
        )
        .await
        .unwrap();

    let app = router(AppState::new(
        db.clone(),
        Arc::new(FakeClock::new(1_000)),
        logs_b, // replica B's OWN LogService — it never saw A's broadcast
    ));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r-sse/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let mut body = resp.into_body().into_data_stream();
    let body = &mut body;

    // …and another chunk AFTER the stream is up (the live part).
    logs_a
        .append(
            &run,
            &StepId("s1".into()),
            &AttemptId("a1".into()),
            b"late\n",
        )
        .await
        .unwrap();

    // Drain frames until both chunks were seen (bounded).
    let mut seen = String::new();
    for _ in 0..20 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.next()).await;
        let Ok(Some(Ok(bytes))) = frame else { break };
        seen.push_str(&String::from_utf8_lossy(bytes.as_ref()));
        if seen.contains("early") && seen.contains("late") {
            break;
        }
    }
    assert!(seen.contains("early"), "replayed chunk served: {seen}");
    assert!(
        seen.contains("late"),
        "LIVE chunk served by the non-tailing replica (durable index): {seen}"
    );
}

fn running_step_with_run(run: &str) -> StepRun {
    StepRun {
        run: RunId(run.into()),
        step: StepId("s1".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    }
}
