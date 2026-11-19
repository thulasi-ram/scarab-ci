//! `scarab logs <run>` against the real server router (in process, on an
//! ephemeral port) over in-memory stores: the bytes it prints, an `error`
//! event, and a reader that goes away.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Db, LogChunkMeta, RunId, RunStatus, StepId, Timestamp,
};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

const KIB: usize = 1024;

/// A server with Run `r1` (in `status`), step `build`, attempt `a1` and the
/// given chunks stored.
async fn serve(
    status: RunStatus,
    chunks: &[Vec<u8>],
) -> (String, Arc<InMemoryDb>, Arc<LogService>) {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("build".into());
    db.seed_run(&run, status);
    db.create_step_run(&run, &step, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.record_attempt(
        &run,
        &step,
        &Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(10),
            failure: None,
            failure_detail: None,
            outcome: if status.is_terminal() {
                AttemptOutcome::Succeeded
            } else {
                AttemptOutcome::Running
            },
        },
    )
    .await
    .unwrap();
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    for c in chunks {
        logs.append(&run, &step, &AttemptId("a1".into()), c)
            .await
            .unwrap();
    }
    let st = AppState::new(
        db.clone(),
        Arc::new(FakeClock::new(1_000_000_000)),
        logs.clone(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(st);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), db, logs)
}

fn scarab_logs(server: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_scarab"));
    c.args(["logs", "r1", "--server", server])
        .stdin(Stdio::null());
    for v in ["SCARAB_SERVER", "SCARAB_TOKEN"] {
        c.env_remove(v);
    }
    c
}

/// Two `data` events whose payloads split one line print the stored bytes,
/// with `\r\n` as `\n`, and nothing added between them.
#[tokio::test(flavor = "multi_thread")]
async fn logs_prints_the_stored_bytes() {
    // 200 KiB + 100 KiB do not fit one 256 KiB batch: two events, the
    // boundary inside "hello world".
    let mut c0 = vec![b'a'; 200 * KIB];
    c0.extend_from_slice(b"\nhello wo");
    let mut c1 = b"rld\r\n".to_vec();
    c1.extend(vec![b'b'; 100 * KIB]);
    c1.push(b'\n');
    let want: Vec<u8> = String::from_utf8([c0.clone(), c1.clone()].concat())
        .unwrap()
        .replace("\r\n", "\n")
        .into_bytes();
    let (server, _, _) = serve(RunStatus::Succeeded, &[c0, c1]).await;

    let mut cmd = scarab_logs(&server);
    let out = tokio::task::spawn_blocking(move || cmd.output().expect("run scarab"))
        .await
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout.len(), want.len());
    assert!(out.stdout == want, "stdout differs from the stored bytes");
}

/// A chunk that will not read after the first batch is an `event: error`:
/// the message goes to stderr and the command exits 1.
#[tokio::test(flavor = "multi_thread")]
async fn logs_exits_1_on_an_error_event() {
    let c0 = vec![b'a'; 300 * KIB];
    let (server, db, _) = serve(RunStatus::Succeeded, &[c0]).await;
    // A second chunk whose body is not in the object store.
    db.append_log_chunk(
        &RunId("r1".into()),
        &StepId("build".into()),
        &AttemptId("a1".into()),
        &LogChunkMeta {
            seq: 1,
            byte_offset: 300 * KIB as u64,
            len: 10,
            object_key: "logs/missing".into(),
        },
    )
    .await
    .unwrap();

    let mut cmd = scarab_logs(&server);
    let out = tokio::task::spawn_blocking(move || cmd.output().expect("run scarab"))
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout.len(), 300 * KIB, "the first batch was printed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("log storage read failed"), "{stderr}");
}

/// `scarab logs <run> | head`: once the reader closes the pipe, the next
/// write ends the command with exit 0, while the Run is still running.
#[tokio::test(flavor = "multi_thread")]
async fn logs_stops_when_its_reader_goes_away() {
    let (server, _, logs) = serve(RunStatus::Running, &[b"first\n".to_vec()]).await;
    let mut child = scarab_logs(&server)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn scarab");
    let mut stdout = child.stdout.take().unwrap();
    let mut first = [0u8; 6];
    tokio::task::spawn_blocking(move || {
        stdout.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"first\n");
        // The reader goes away.
        drop(stdout);
    })
    .await
    .unwrap();

    // More output arrives; writing it fails, and the command ends.
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        logs.append(
            &RunId("r1".into()),
            &StepId("build".into()),
            &AttemptId("a1".into()),
            b"more\n",
        )
        .await
        .unwrap();
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("scarab logs kept running after its reader went away");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(status.code(), Some(0));
}
