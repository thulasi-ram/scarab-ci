//! Log streaming over HTTP (ADR-0013): batched replay with cursor ids, resume
//! from `?from=` / `Last-Event-ID`, the `?tail=` cut, UTF-8 characters split
//! across chunks, and the raw download. Hermetic: InMemoryDb + an in-memory
//! object store.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{Attempt, AttemptId, AttemptOutcome, Db, RunId, RunStatus, StepId, Timestamp};
use scarab_server::log_stream::{Item, LogStream, Scope, Start, BATCH_BYTES, SETTLE};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

const KIB: usize = 1024;

/// A settled run `r1` with step `build` and the given attempts, each with its
/// chunks appended in order. Settled, so every stream replays and closes.
async fn app_with(attempts: &[(&str, Vec<Vec<u8>>)]) -> axum::Router {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("build".into());
    db.seed_run(&run, RunStatus::Succeeded);
    db.create_step_run(&run, &step, None, &[], Timestamp(0))
        .await
        .unwrap();
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    for (i, (id, chunks)) in attempts.iter().enumerate() {
        let aid = AttemptId((*id).into());
        db.record_attempt(
            &run,
            &step,
            &Attempt {
                id: aid.clone(),
                started_at: Timestamp(10 * (i as i64 + 1)),
                failure: None,
                failure_detail: None,
                outcome: AttemptOutcome::Succeeded,
            },
        )
        .await
        .unwrap();
        for c in chunks {
            logs.append(&run, &step, &aid, c).await.unwrap();
        }
    }
    router(AppState::new(db, Arc::new(FakeClock::new(1_000)), logs))
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Ev {
    event: Option<String>,
    id: Option<String>,
    data: String,
}

/// Split an SSE body into events. A line ends in CRLF, LF or CR, as an
/// EventSource reads it; axum writes a `\r` in data as a line end of its own.
fn events(body: &str) -> Vec<Ev> {
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    body.split("\n\n")
        .filter(|b| !b.is_empty())
        .filter_map(|block| {
            let mut ev = Ev::default();
            let mut data: Vec<&str> = Vec::new();
            for line in block.split('\n') {
                if line.starts_with(':') {
                    continue;
                }
                let (f, v) = line.split_once(':').unwrap_or((line, ""));
                let v = v.strip_prefix(' ').unwrap_or(v);
                match f {
                    "data" => data.push(v),
                    "event" => ev.event = Some(v.into()),
                    "id" => ev.id = Some(v.into()),
                    _ => {}
                }
            }
            if data.is_empty() && ev.id.is_none() {
                return None;
            }
            ev.data = data.join("\n");
            Some(ev)
        })
        .collect()
}

async fn get(app: &axum::Router, uri: &str, last_event_id: Option<&str>) -> (StatusCode, String) {
    let mut req = Request::builder().uri(uri);
    if let Some(id) = last_event_id {
        req = req.header("last-event-id", id);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn data_of(evs: &[Ev]) -> String {
    evs.iter()
        .filter(|e| e.event.is_none())
        .map(|e| e.data.as_str())
        .collect()
}

fn fill(c: u8, n: usize) -> Vec<u8> {
    let mut v = vec![c; n - 1];
    v.push(b'\n');
    v
}

/// a1: three 100 KiB chunks; a2: one 300 KiB chunk and a small one.
async fn two_attempts() -> (axum::Router, String) {
    let a1 = vec![
        fill(b'a', 100 * KIB),
        fill(b'b', 100 * KIB),
        fill(b'c', 100 * KIB),
    ];
    let a2 = vec![fill(b'd', 300 * KIB), b"tail\n".to_vec()];
    let full: Vec<u8> = a1.iter().chain(a2.iter()).flatten().copied().collect();
    let app = app_with(&[("a1", a1), ("a2", a2)]).await;
    (app, String::from_utf8(full).unwrap())
}

#[tokio::test]
async fn replay_is_batched_by_whole_chunks_with_cursor_ids() {
    let (app, full) = two_attempts().await;
    let (status, body) = get(&app, "/v1/runs/r1/steps/build/logs", None).await;
    assert_eq!(status, StatusCode::OK);
    let evs = events(&body);
    let ids: Vec<_> = evs.iter().map(|e| e.id.clone().unwrap()).collect();
    // 100+100 KiB fit one 256 KiB batch, the third does not; the 300 KiB chunk
    // goes alone; a batch never mixes attempts.
    assert_eq!(ids, ["a1:2", "a1:3", "a2:1", "a2:2"]);
    assert_eq!(evs[0].data.len(), 200 * KIB);
    assert_eq!(data_of(&evs), full);

    // The run-wide route streams the same bytes with the same ids.
    let (_, body) = get(&app, "/v1/runs/r1/logs", None).await;
    let run_wide = events(&body);
    assert_eq!(data_of(&run_wide), full);
    assert_eq!(
        run_wide
            .iter()
            .filter_map(|e| e.id.clone())
            .collect::<Vec<_>>(),
        ids
    );
}

#[tokio::test]
async fn resume_from_any_id_replays_exactly_the_rest() {
    let (app, full) = two_attempts().await;
    let (_, body) = get(&app, "/v1/runs/r1/steps/build/logs", None).await;
    let evs = events(&body);
    for (i, ev) in evs.iter().enumerate() {
        let id = ev.id.as_deref().unwrap();
        let rest: String = data_of(&evs[i + 1..]);
        assert_eq!(rest.len() + data_of(&evs[..=i]).len(), full.len());

        let (status, body) = get(
            &app,
            &format!("/v1/runs/r1/steps/build/logs?from={id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(data_of(&events(&body)), rest, "?from={id}");

        let (status, body) = get(&app, "/v1/runs/r1/steps/build/logs", Some(id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(data_of(&events(&body)), rest, "Last-Event-ID {id}");
    }
    // `?from=` wins over the header, and a cursor ignores `?tail=`.
    let (_, body) = get(
        &app,
        "/v1/runs/r1/steps/build/logs?from=a2:1&tail=1",
        Some("a1:0"),
    )
    .await;
    let evs = events(&body);
    assert!(
        evs.iter().all(|e| e.event.is_none()),
        "no truncated event with a cursor"
    );
    assert_eq!(data_of(&evs), "tail\n");
}

#[tokio::test]
async fn a_bad_cursor_is_400_not_a_full_replay() {
    let (app, _) = two_attempts().await;
    for uri in [
        "/v1/runs/r1/steps/build/logs?from=garbage",
        "/v1/runs/r1/steps/build/logs?from=a1:x",
        "/v1/runs/r1/steps/build/logs?from=ghost:0",
        // a1 is real, but outside the `?attempt=a2` selection.
        "/v1/runs/r1/steps/build/logs?attempt=a2&from=a1:0",
    ] {
        let (status, _) = get(&app, uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
    let (status, _) = get(&app, "/v1/runs/r1/steps/build/logs", Some("nope")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tail_starts_at_a_chunk_boundary_and_reports_the_cut() {
    // a1: 10 + 20 bytes; a2: 30 + 40 bytes. Total 100.
    let app = app_with(&[
        ("a1", vec![fill(b'a', 10), fill(b'b', 20)]),
        ("a2", vec![fill(b'c', 30), fill(b'd', 40)]),
    ])
    .await;
    let text = |parts: &[Vec<u8>]| String::from_utf8(parts.concat()).unwrap();
    let cases = [
        // 40 + 30 fit in 75, + 20 does not: a1 is skipped whole.
        (75, 30, text(&[fill(b'c', 30), fill(b'd', 40)])),
        // 40 + 30 + 20 = 90 fit in 95; the cut lands inside a1.
        (
            95,
            10,
            text(&[fill(b'b', 20), fill(b'c', 30), fill(b'd', 40)]),
        ),
        // The last chunk alone is bigger than the tail: it is sent.
        (5, 60, text(&[fill(b'd', 40)])),
    ];
    for (tail, skipped, kept) in cases {
        let (status, body) = get(
            &app,
            &format!("/v1/runs/r1/steps/build/logs?tail={tail}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let evs = events(&body);
        assert_eq!(evs[0].event.as_deref(), Some("truncated"), "tail={tail}");
        let cut: serde_json::Value = serde_json::from_str(&evs[0].data).unwrap();
        assert_eq!(
            cut,
            serde_json::json!({"skipped_bytes": skipped, "total_bytes": 100})
        );
        assert_eq!(data_of(&evs[1..]), kept, "tail={tail}");
    }
    // A tail the log fits in: no truncated event, everything replays.
    let (_, body) = get(&app, "/v1/runs/r1/steps/build/logs?tail=100", None).await;
    let evs = events(&body);
    assert!(evs.iter().all(|e| e.event.is_none()));
    assert_eq!(data_of(&evs).len(), 100);
}

#[tokio::test]
async fn a_character_split_across_chunks_arrives_whole() {
    let euro = "€".as_bytes(); // E2 82 AC
                               // Without the UTF-8 rule the batch would end after the first chunk
                               // (200 KiB + 100 KiB > 256 KiB), between the euro sign's bytes.
    let mut c0 = vec![b'a'; 200 * KIB];
    c0.extend_from_slice(&euro[..2]);
    let mut c1 = euro[2..].to_vec();
    c1.extend(fill(b'b', 100 * KIB));
    // The `?tail=6` cut below starts at c3, on the second byte of a euro sign.
    let c2 = euro[..1].to_vec();
    let mut c3 = euro[1..].to_vec();
    c3.extend_from_slice(b"end\n");
    let full: Vec<u8> = [c0.clone(), c1.clone(), c2.clone(), c3.clone()].concat();
    let app = app_with(&[("a1", vec![c0, c1, c2, c3])]).await;

    let (_, body) = get(&app, "/v1/runs/r1/steps/build/logs", None).await;
    let text = data_of(&events(&body));
    assert!(!text.contains('\u{FFFD}'));
    assert_eq!(text.as_bytes(), full.as_slice());

    // A tail cut that starts on a continuation byte drops it, not mangles it.
    let (_, body) = get(&app, "/v1/runs/r1/steps/build/logs?tail=6", None).await;
    let evs = events(&body);
    assert_eq!(evs[0].event.as_deref(), Some("truncated"));
    assert_eq!(data_of(&evs[1..]), "end\n");
}

#[tokio::test]
async fn raw_download_streams_the_whole_try_as_an_attachment() {
    let (app, full) = two_attempts().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/steps/build/logs/raw?attempt=a2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();
    assert_eq!(h["content-type"], "text/plain; charset=utf-8");
    assert_eq!(
        h["content-disposition"],
        "attachment; filename=\"scarab-r1-try2.log\""
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let a1_len = 300 * KIB;
    assert_eq!(&bytes[..], full[a1_len..].as_bytes());

    let (status, _) = get(&app, "/v1/runs/r1/steps/build/logs/raw", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "the attempt is required");
    let (status, _) = get(&app, "/v1/runs/r1/steps/build/logs/raw?attempt=ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&app, "/v1/runs/r1/steps/ghost/logs/raw?attempt=a1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn raw(app: &axum::Router, attempt: &str) -> Vec<u8> {
    let (status, body) = get(
        app,
        &format!("/v1/runs/r1/steps/build/logs/raw?attempt={attempt}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body.into_bytes()
}

#[tokio::test]
async fn crlf_streams_as_one_line_end_even_split_across_chunks() {
    // c0 | c1 is a batch boundary (200 KiB + 100 KiB > 256 KiB) inside a
    // `\r\n`; c1 | c2 splits one inside a batch; c1 also holds a whole one.
    let mut c0 = vec![b'x'; 200 * KIB];
    c0.push(b'\r');
    let mut c1 = b"\n".to_vec();
    c1.extend(fill(b'y', 100 * KIB));
    c1.extend_from_slice(b"one\r\ntwo\r");
    let c2 = b"\nthree\n".to_vec();
    let full: Vec<u8> = [c0.clone(), c1.clone(), c2.clone()].concat();
    let want = String::from_utf8(full.clone())
        .unwrap()
        .replace("\r\n", "\n");
    let app = app_with(&[("a1", vec![c0, c1, c2])]).await;

    let (_, body) = get(&app, "/v1/runs/r1/steps/build/logs", None).await;
    let evs = events(&body);
    assert_eq!(
        evs.iter().filter_map(|e| e.id.clone()).collect::<Vec<_>>(),
        ["a1:1", "a1:3"]
    );
    assert_eq!(data_of(&evs), want);
    // No `\r\n` reaches the wire, so no reader sees a blank line inside one.
    assert!(!body.contains("\r\n"));

    // A stream resumed between the `\r` and the `\n` does not send the `\n`
    // again as a line of its own.
    let (_, body) = get(&app, "/v1/runs/r1/steps/build/logs?from=a1:1", None).await;
    assert_eq!(data_of(&events(&body)), want[evs[0].data.len()..]);

    // The raw download keeps the stored bytes.
    assert_eq!(raw(&app, "a1").await, full);
}

#[tokio::test]
async fn a_lone_cr_reaches_the_reader_as_a_line_end() {
    let chunks = vec![b"50%\r60%\r".to_vec(), b"done\n".to_vec()];
    let app = app_with(&[("a1", chunks.clone())]).await;
    let (_, body) = get(&app, "/v1/runs/r1/steps/build/logs", None).await;
    assert_eq!(data_of(&events(&body)), "50%\n60%\ndone\n");
    assert_eq!(raw(&app, "a1").await, chunks.concat());
}

// --- Bounds and live polls ----------------------------------------------------

/// A finished log whose every chunk ends in a lone UTF-8 lead byte is not
/// read into one buffer: each batch takes at most one chunk past
/// `BATCH_BYTES`, the ids advance, and a resume from any id repeats nothing.
#[tokio::test]
async fn chunks_that_all_end_inside_a_character_still_come_in_bounded_batches() {
    const CHUNK: usize = 128 * KIB;
    let chunks: Vec<Vec<u8>> = (0..8).map(|_| vec![0xE2; CHUNK]).collect();
    let app = app_with(&[("a1", chunks)]).await;

    let (status, body) = get(&app, "/v1/runs/r1/steps/build/logs", None).await;
    assert_eq!(status, StatusCode::OK);
    let evs = events(&body);
    assert!(evs.len() > 1, "{} data events", evs.len());
    let max = BATCH_BYTES as usize + 2 * CHUNK;
    let mut seq = 0u64;
    for ev in &evs {
        let id = ev.id.as_deref().unwrap();
        let next: u64 = id.strip_prefix("a1:").unwrap().parse().unwrap();
        assert!(next > seq, "ids advance: {id} after a1:{seq}");
        assert!(
            (next - seq) as usize * CHUNK <= max,
            "{id} carries {} stored bytes",
            (next - seq) as usize * CHUNK
        );
        seq = next;
    }
    assert_eq!(seq, 8);
    // Each lone lead byte goes out as one replacement character.
    assert_eq!(data_of(&evs), "\u{FFFD}".repeat(8 * CHUNK));

    for ev in &evs {
        let id = ev.id.as_deref().unwrap();
        let from: usize = id.strip_prefix("a1:").unwrap().parse().unwrap();
        let (status, body) = get(
            &app,
            &format!("/v1/runs/r1/steps/build/logs?from={id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            data_of(&events(&body)),
            "\u{FFFD}".repeat((8 - from) * CHUNK),
            "?from={id}"
        );
    }
}

/// Run `r1`, step `build`, with the Run's status (`None`: no Run row) and the
/// given attempts, each with its outcome and its chunks.
async fn live_store(
    status: Option<RunStatus>,
    attempts: &[(&str, AttemptOutcome, Vec<Vec<u8>>)],
) -> (Arc<InMemoryDb>, Arc<LogService>) {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("build".into());
    if let Some(s) = status {
        db.seed_run(&run, s);
    }
    db.create_step_run(&run, &step, None, &[], Timestamp(0))
        .await
        .unwrap();
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    for (i, (id, outcome, chunks)) in attempts.iter().enumerate() {
        let aid = AttemptId((*id).into());
        db.record_attempt(
            &run,
            &step,
            &Attempt {
                id: aid.clone(),
                started_at: Timestamp(10 * (i as i64 + 1)),
                failure: None,
                failure_detail: None,
                outcome: *outcome,
            },
        )
        .await
        .unwrap();
        for c in chunks {
            logs.append(&run, &step, &aid, c).await.unwrap();
        }
    }
    (db, logs)
}

/// A live stream of step `build`'s attempts.
async fn open_live(db: &Arc<InMemoryDb>, logs: &Arc<LogService>, start: Start) -> LogStream {
    let step = StepId("build".into());
    let opened = LogStream::open(
        logs.clone(),
        db.clone() as Arc<dyn Db>,
        RunId("r1".into()),
        Scope::Step {
            step: step.clone(),
            read_step: step,
            pinned: None,
        },
        start,
        true,
    )
    .await;
    match opened {
        Ok(s) => s,
        Err(_) => panic!("the stream did not open"),
    }
}

/// Everything the stream has before it would wait for a live poll.
async fn opening(s: &mut LogStream) -> Vec<Item> {
    let mut out = Vec::new();
    while let Some(item) = s.next(false).await.unwrap() {
        out.push(item);
    }
    out
}

/// A running attempt's newest chunk that ends inside a character, with
/// nothing appended after it, is held for at most two passes and then sent
/// with the cut byte replaced. It starts at a tail cut on a continuation
/// byte; that byte is still dropped when the held batch goes out.
#[tokio::test(start_paused = true)]
async fn a_held_back_chunk_is_sent_after_two_passes() {
    let (db, logs) = live_store(
        Some(RunStatus::Running),
        &[(
            "a1",
            AttemptOutcome::Running,
            vec![b"hello\n".to_vec(), vec![0x82, 0xAC, b'x', 0xE2]],
        )],
    )
    .await;
    let mut s = open_live(&db, &logs, Start::Tail(4)).await;
    let head = opening(&mut s).await;
    assert!(
        matches!(head.as_slice(), [Item::Truncated { .. }]),
        "{head:?}"
    );

    let mut polls = 0;
    let data = loop {
        assert!(polls < 3, "no data event within 3 polls");
        match s.next(true).await.unwrap() {
            Some(Item::Keepalive) => polls += 1,
            Some(item) => break item,
            None => panic!("the stream ended"),
        }
    };
    assert_eq!(
        data,
        Item::Data {
            id: "a1:2".into(),
            text: "x\u{FFFD}".into(),
        }
    );
    // Sent once: later polls do not read it again.
    for _ in 0..3 {
        assert_eq!(s.next(true).await.unwrap(), Some(Item::Keepalive));
    }
}

/// A finished attempt is not held back while the Run goes on: its last chunk,
/// cut inside a character, is sent on the first pass.
#[tokio::test(start_paused = true)]
async fn a_finished_attempt_is_not_held_back() {
    let (db, logs) = live_store(
        Some(RunStatus::Running),
        &[("a1", AttemptOutcome::Failed, vec![vec![b'x', 0xE2]])],
    )
    .await;
    let mut s = open_live(&db, &logs, Start::Full).await;
    assert_eq!(
        opening(&mut s).await,
        [Item::Data {
            id: "a1:1".into(),
            text: "x\u{FFFD}".into(),
        }]
    );
}

/// A live stream ends one pass after the Run settles, and not before.
#[tokio::test(start_paused = true)]
async fn a_live_stream_ends_one_pass_after_the_run_settles() {
    let (db, logs) = live_store(
        Some(RunStatus::Running),
        &[("a1", AttemptOutcome::Running, vec![b"one\n".to_vec()])],
    )
    .await;
    let mut s = open_live(&db, &logs, Start::Full).await;
    assert_eq!(opening(&mut s).await.len(), 1);
    for _ in 0..3 {
        assert_eq!(s.next(true).await.unwrap(), Some(Item::Keepalive));
    }
    // Output stored before the Run settled still goes out on the last pass.
    logs.append(
        &RunId("r1".into()),
        &StepId("build".into()),
        &AttemptId("a1".into()),
        b"two\n",
    )
    .await
    .unwrap();
    db.seed_run(&RunId("r1".into()), RunStatus::Succeeded);
    assert_eq!(
        s.next(true).await.unwrap(),
        Some(Item::Data {
            id: "a1:2".into(),
            text: "two\n".into(),
        })
    );
    assert_eq!(s.next(true).await.unwrap(), None);
}

/// A Run row that is gone ends the stream; it does not poll for ever.
#[tokio::test(start_paused = true)]
async fn a_stream_whose_run_row_is_gone_ends() {
    let (db, logs) = live_store(
        None,
        &[("a1", AttemptOutcome::Running, vec![b"one\n".to_vec()])],
    )
    .await;
    let mut s = open_live(&db, &logs, Start::Full).await;
    assert_eq!(opening(&mut s).await.len(), 1);
    assert_eq!(s.next(true).await.unwrap(), None);
}

/// An attempt that is no longer running, sent in full and quiet for
/// `SETTLE`, stops being read on every poll; the running one still is.
#[tokio::test(start_paused = true)]
async fn a_settled_attempt_index_is_not_read_on_every_poll() {
    let (db, logs) = live_store(
        Some(RunStatus::Running),
        &[
            ("a1", AttemptOutcome::Failed, vec![b"one\n".to_vec()]),
            ("a2", AttemptOutcome::Running, vec![b"two\n".to_vec()]),
        ],
    )
    .await;
    let mut s = open_live(&db, &logs, Start::Full).await;
    assert_eq!(opening(&mut s).await.len(), 2);
    let start = tokio::time::Instant::now();
    while start.elapsed() < SETTLE + std::time::Duration::from_secs(2) {
        assert_eq!(s.next(true).await.unwrap(), Some(Item::Keepalive));
    }
    let before = s.index_reads();
    for _ in 0..3 {
        assert_eq!(s.next(true).await.unwrap(), Some(Item::Keepalive));
    }
    assert_eq!(
        s.index_reads() - before,
        3,
        "one index read per poll, for the running attempt only"
    );
    // The running attempt is still followed.
    logs.append(
        &RunId("r1".into()),
        &StepId("build".into()),
        &AttemptId("a2".into()),
        b"three\n",
    )
    .await
    .unwrap();
    assert_eq!(
        s.next(true).await.unwrap(),
        Some(Item::Data {
            id: "a2:2".into(),
            text: "three\n".into(),
        })
    );
}
