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

/// The tail cut starts at the chunk that holds the `tail`-th byte from the
/// end: at least `min(tail, total)` bytes are sent, and less than one chunk
/// more. A cut after that chunk sent up to a chunk less than `tail`, so a log
/// stored in big chunks showed much less than the notice said.
#[tokio::test]
async fn tail_starts_at_the_chunk_that_holds_the_cut_and_reports_it() {
    // a1: 10 + 20 bytes; a2: 30 + 40 bytes. Total 100.
    let app = app_with(&[
        ("a1", vec![fill(b'a', 10), fill(b'b', 20)]),
        ("a2", vec![fill(b'c', 30), fill(b'd', 40)]),
    ])
    .await;
    let text = |parts: &[Vec<u8>]| String::from_utf8(parts.concat()).unwrap();
    let cases = [
        // 40 + 30 is exactly 70: a1 is skipped whole.
        (70, 30, text(&[fill(b'c', 30), fill(b'd', 40)])),
        // The 75th byte from the end is in a1's second chunk: it is sent.
        (
            75,
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
        let sent = data_of(&evs[1..]);
        assert_eq!(sent, kept, "tail={tail}");
        assert!(sent.len() >= tail, "tail={tail}: {} bytes sent", sent.len());
    }
    // A tail the log fits in, or whose cut lands in the first chunk: no
    // truncated event, everything replays.
    for tail in [100, 95] {
        let (_, body) = get(
            &app,
            &format!("/v1/runs/r1/steps/build/logs?tail={tail}"),
            None,
        )
        .await;
        let evs = events(&body);
        assert!(evs.iter().all(|e| e.event.is_none()), "tail={tail}");
        assert_eq!(data_of(&evs).len(), 100, "tail={tail}");
    }
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

    // No `attempt`: the latest one, a2.
    let (status, body) = get(&app, "/v1/runs/r1/steps/build/logs/raw", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), full[a1_len..].as_bytes());
    // A number is not an attempt id: the 404 names the ids that are.
    let (status, body) = get(&app, "/v1/runs/r1/steps/build/logs/raw?attempt=1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.contains("attempt id") && body.contains("a1, a2"),
        "{body}"
    );
    let (status, _) = get(&app, "/v1/runs/r1/steps/build/logs/raw?attempt=ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&app, "/v1/runs/r1/steps/ghost/logs/raw?attempt=a1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// L15: the raw download is a step's own bytes as `text/plain`; with
/// `nosniff` a browser does not read them as HTML even when they look like it.
#[tokio::test]
async fn raw_download_tells_the_browser_not_to_sniff_the_type() {
    let app = app_with(&[("a1", vec![b"<html><script>alert(1)</script>\n".to_vec()])]).await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/steps/build/logs/raw?attempt=a1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["content-type"], "text/plain; charset=utf-8");
}

/// An object store whose reads take longer for earlier keys, so fetches in
/// flight together finish in reverse order. Counts the reads in flight.
struct SlowStore {
    inner: InMemoryObjectStore,
    in_flight: std::sync::atomic::AtomicUsize,
    most_in_flight: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl scarab_storage::ObjectStore for SlowStore {
    async fn get(&self, key: &str) -> Result<Vec<u8>, scarab_storage::StorageError> {
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.in_flight.fetch_add(1, SeqCst) + 1;
        self.most_in_flight.fetch_max(now, SeqCst);
        // logs/r1/build/a1/00000123.gz: the seq sets the delay.
        let seq: u64 = key
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_suffix(".gz"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        tokio::time::sleep(std::time::Duration::from_millis(20 - seq % 20)).await;
        let out = self.inner.get(key).await;
        self.in_flight.fetch_sub(1, SeqCst);
        out
    }
    async fn put(&self, key: &str, data: Vec<u8>) -> Result<(), scarab_storage::StorageError> {
        self.inner.put(key, data).await
    }
    async fn delete(&self, key: &str) -> Result<(), scarab_storage::StorageError> {
        self.inner.delete(key).await
    }
    async fn list_objects(
        &self,
        prefix: &str,
    ) -> Result<Vec<scarab_storage::StoredObject>, scarab_storage::StorageError> {
        self.inner.list_objects(prefix).await
    }
}

/// A log tailed in many small chunks downloads with the next chunks fetched
/// ahead, at most `PREFETCH_CHUNKS` at once, and still in order: the body is
/// the stored bytes exactly, though later fetches finish first. The SSE
/// replay of the same log is the same bytes.
#[tokio::test(start_paused = true)]
async fn raw_download_of_many_small_chunks_is_prefetched_in_order() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("build".into());
    let attempt = AttemptId("a1".into());
    db.seed_run(&run, RunStatus::Succeeded);
    db.create_step_run(&run, &step, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.record_attempt(
        &run,
        &step,
        &Attempt {
            id: attempt.clone(),
            started_at: Timestamp(10),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Succeeded,
        },
    )
    .await
    .unwrap();
    let store = Arc::new(SlowStore {
        inner: InMemoryObjectStore::new(),
        in_flight: Default::default(),
        most_in_flight: Default::default(),
    });
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));
    let mut want = Vec::new();
    for i in 0..500 {
        let line = format!("line {i:04} {}\n", "x".repeat(i % 7));
        logs.append(&run, &step, &attempt, line.as_bytes())
            .await
            .unwrap();
        want.extend_from_slice(line.as_bytes());
    }
    let app = router(AppState::new(db, Arc::new(FakeClock::new(1_000)), logs));

    let body = raw(&app, "a1").await;
    assert_eq!(body, want, "every chunk, in seq order");
    let most = store
        .most_in_flight
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        most > 1 && most <= scarab_server::logs::PREFETCH_CHUNKS,
        "{most} reads in flight"
    );

    let (_, sse) = get(&app, "/v1/runs/r1/steps/build/logs", None).await;
    assert_eq!(data_of(&events(&sse)).as_bytes(), &want[..]);
}

/// 5ca1f3c: a shared service's whole log downloads like a step try's: the
/// current Take by default, `?take=` for an older one, `text/plain` with
/// `nosniff`, an ASCII filename, and 404 for an unknown run, service or Take.
#[tokio::test]
async fn service_raw_download_streams_the_whole_instance_log_as_an_attachment() {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let run = RunId("r1".into());
    db.seed_run(&run, RunStatus::Succeeded);
    // "db" at Takes 1 and 2; the name holds a character a filename must not.
    let name = "db ü";
    for take in [1, 2] {
        db.create_run_service(None, &run, take, name, Timestamp(0))
            .await
            .unwrap();
    }
    let (s1, a1) = scarab_server::logs::service_stream_key(name, 1);
    logs.append(&run, &s1, &a1, b"take one\n").await.unwrap();
    let (s2, a2) = scarab_server::logs::service_stream_key(name, 2);
    // Several chunks, one bigger than a stream batch, all in the body.
    let big = fill(b'x', 300 * KIB);
    for c in [b"<html>ready\n".to_vec(), big.clone(), b"end\n".to_vec()] {
        logs.append(&run, &s2, &a2, &c).await.unwrap();
    }
    let app = router(AppState::new(db, Arc::new(FakeClock::new(1_000)), logs));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/services/db%20%C3%BC/logs/raw")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();
    assert_eq!(h["content-type"], "text/plain; charset=utf-8");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(
        h["content-disposition"],
        "attachment; filename=\"scarab-r1-db__-take2.log\""
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let mut want = b"<html>ready\n".to_vec();
    want.extend_from_slice(&big);
    want.extend_from_slice(b"end\n");
    assert_eq!(&bytes[..], &want[..], "the current Take, every chunk");

    let (status, body) = get(
        &app,
        "/v1/runs/r1/services/db%20%C3%BC/logs/raw?take=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "take one\n");

    for uri in [
        "/v1/runs/r1/services/ghost/logs/raw",
        "/v1/runs/r1/services/db%20%C3%BC/logs/raw?take=3",
        "/v1/runs/nope/services/db%20%C3%BC/logs/raw",
    ] {
        let (status, _) = get(&app, uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
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

/// The hold is counted in passes, not reads: a pass that sends the whole
/// chunks before the cut one reads the source a second time, and that read
/// does not count as a pass. With nothing appended, the first live poll still
/// holds the chunk; the second sends it with the cut byte replaced.
#[tokio::test(start_paused = true)]
async fn a_held_back_chunk_waits_two_polls_after_whole_chunks_were_sent() {
    let (db, logs) = live_store(
        Some(RunStatus::Running),
        &[(
            "a1",
            AttemptOutcome::Running,
            vec![b"hello\n".to_vec(), vec![b'x', 0xE2]],
        )],
    )
    .await;
    let mut s = open_live(&db, &logs, Start::Full).await;
    assert_eq!(
        opening(&mut s).await,
        [Item::Data {
            id: "a1:1".into(),
            text: "hello\n".into(),
        }]
    );
    assert_eq!(
        s.next(true).await.unwrap(),
        Some(Item::Keepalive),
        "held on the first poll"
    );
    assert_eq!(
        s.next(true).await.unwrap(),
        Some(Item::Data {
            id: "a1:2".into(),
            text: "x\u{FFFD}".into(),
        }),
        "sent on the second poll"
    );
}

/// REV-m11: a running attempt's newest chunk ends inside a character and
/// is held back; a marker is stored after it. The held bytes go out before
/// the marker, with the cut character as U+FFFD, never dropped. Kills: the
/// hold cleared when a marker comes next without sending what it held.
#[tokio::test(start_paused = true)]
async fn held_bytes_before_a_marker_are_sent_with_a_replacement_character() {
    let (db, logs) = live_store(
        Some(RunStatus::Running),
        &[(
            "a1",
            AttemptOutcome::Running,
            vec![b"hello\n".to_vec(), vec![b'x', 0xE2, 0x82]],
        )],
    )
    .await;
    let mut s = open_live(&db, &logs, Start::Full).await;
    assert_eq!(
        opening(&mut s).await,
        [Item::Data {
            id: "a1:1".into(),
            text: "hello\n".into(),
        }]
    );
    assert_eq!(
        s.next(true).await.unwrap(),
        Some(Item::Keepalive),
        "held on the first poll"
    );
    // The tail stores a marker: the rest of the character never comes.
    let stream = scarab_engine::LogStreamId::new(
        &RunId("r1".into()),
        &StepId("build".into()),
        &AttemptId("a1".into()),
    );
    let resource = scarab_engine::LogLease::Tail.resource(&stream);
    let lease = db.lease(&resource, "w", 1).await.unwrap();
    let writer = scarab_engine::LogWriter::holding(
        scarab_engine::LogLease::Tail,
        &lease.held_by(&resource, "w").unwrap(),
    );
    logs.append_marker(
        &stream,
        scarab_engine::LogRowKind::SourceRestarted,
        None,
        &writer,
        None,
        Timestamp(1),
    )
    .await
    .unwrap();
    let mut items = Vec::new();
    while items.len() < 2 {
        match s.next(true).await.unwrap() {
            Some(Item::Keepalive) => {}
            Some(item) => items.push(item),
            None => panic!("the stream ended"),
        }
    }
    assert_eq!(
        items[0],
        Item::Data {
            id: "a1:2".into(),
            text: "x\u{FFFD}".into(),
        }
    );
    assert!(
        matches!(
            &items[1],
            Item::Marker {
                kind: scarab_engine::LogRowKind::SourceRestarted,
                ..
            }
        ),
        "{:?}",
        items[1]
    );
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

/// The stream of an attempt whose log is still being stored (its tail lease
/// is live) does not end when the run has settled: it opens live, says the
/// log is being stored, follows the chunks still stored, and says when that
/// stops and ends one pass after the lease is given up.
#[tokio::test(start_paused = true)]
async fn a_stream_does_not_end_while_the_log_is_still_being_stored() {
    let (db, logs) = live_store(
        Some(RunStatus::Succeeded),
        &[("a1", AttemptOutcome::Succeeded, vec![b"one\n".to_vec()])],
    )
    .await;
    let run = RunId("r1".into());
    let step = StepId("build".into());
    let attempt = AttemptId("a1".into());
    let resource = scarab_server::log_tail::tail_lease_resource(&run, &step, &attempt);
    assert_eq!(resource, "tail2:r1:build:a1");
    db.lease(&resource, "replica-a", 45_000).await.unwrap();

    // The route opens a settled run's stream as a replay (`live` false).
    let opened = LogStream::open(
        logs.clone(),
        db.clone() as Arc<dyn Db>,
        run.clone(),
        Scope::Step {
            step: step.clone(),
            read_step: step.clone(),
            pinned: Some(attempt.clone()),
        },
        Start::Full,
        false,
    )
    .await;
    let Ok(mut s) = opened else {
        panic!("the stream did not open")
    };
    assert_eq!(
        opening(&mut s).await,
        [
            Item::Storing(true),
            Item::Data {
                id: "a1:1".into(),
                text: "one\n".into(),
            }
        ]
    );
    for _ in 0..3 {
        assert_eq!(
            s.next(true).await.unwrap(),
            Some(Item::Keepalive),
            "still being stored"
        );
    }
    logs.append(&run, &step, &attempt, b"two\n").await.unwrap();
    assert_eq!(
        s.next(true).await.unwrap(),
        Some(Item::Data {
            id: "a1:2".into(),
            text: "two\n".into(),
        })
    );
    // Quiet past SETTLE: still followed while the lease is live.
    let start = tokio::time::Instant::now();
    while start.elapsed() < SETTLE + std::time::Duration::from_secs(2) {
        assert_eq!(s.next(true).await.unwrap(), Some(Item::Keepalive));
    }
    logs.append(&run, &step, &attempt, b"three\n")
        .await
        .unwrap();
    db.release_lease(&resource, "replica-a").await.unwrap();
    assert_eq!(s.next(true).await.unwrap(), Some(Item::Storing(false)));
    assert_eq!(
        s.next(true).await.unwrap(),
        Some(Item::Data {
            id: "a1:3".into(),
            text: "three\n".into(),
        }),
        "what was stored before the lease was given up still goes out"
    );
    assert_eq!(s.next(true).await.unwrap(), None);
}

/// A stream that waits on a tail lease that is never given up (a tail that
/// hangs) ends `STORE_WAIT` after it would have ended, and says so.
#[tokio::test(start_paused = true)]
async fn a_stream_waits_for_a_stuck_lease_at_most_store_wait() {
    let (db, logs) = live_store(
        Some(RunStatus::Succeeded),
        &[("a1", AttemptOutcome::Succeeded, vec![b"one\n".to_vec()])],
    )
    .await;
    db.lease("tail:r1:build:a1", "stuck", 3_600_000)
        .await
        .unwrap();
    let mut s = open_live(&db, &logs, Start::Full).await;
    assert_eq!(opening(&mut s).await.len(), 2, "storing, then the data");
    let start = tokio::time::Instant::now();
    let mut wait_over = 0;
    loop {
        match s.next(true).await.unwrap() {
            Some(Item::Keepalive) => {}
            Some(Item::StoreWaitOver) => wait_over += 1,
            None => break,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(wait_over, 1, "the stream says it stopped waiting, once");
    let waited = start.elapsed();
    assert!(
        waited >= scarab_server::log_stream::STORE_WAIT
            && waited < scarab_server::log_stream::STORE_WAIT + std::time::Duration::from_secs(5),
        "{waited:?}"
    );
}

/// The running attempt's tail holds its lease for as long as it reads: that
/// is not "still being stored". A running step's stream sends no `storing`
/// event, and its log is followed as before.
#[tokio::test(start_paused = true)]
async fn a_running_attempt_with_a_live_tail_lease_is_not_storing() {
    let (db, logs) = live_store(
        Some(RunStatus::Running),
        &[("a1", AttemptOutcome::Running, vec![b"one\n".to_vec()])],
    )
    .await;
    db.lease("tail:r1:build:a1", "replica-a", 3_600_000)
        .await
        .unwrap();
    let mut s = open_live(&db, &logs, Start::Full).await;
    assert_eq!(
        opening(&mut s).await,
        [Item::Data {
            id: "a1:1".into(),
            text: "one\n".into(),
        }]
    );
    for _ in 0..3 {
        assert_eq!(s.next(true).await.unwrap(), Some(Item::Keepalive));
    }
}

/// A settled run whose `build`/`a1` log is `one\r`, a `source_restarted`
/// marker, `\nmore\n`, then an `incomplete` marker closing it.
async fn app_with_markers() -> (axum::Router, Arc<InMemoryDb>) {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("build".into());
    let attempt = AttemptId("a1".into());
    db.seed_run(&run, RunStatus::Succeeded);
    db.create_step_run(&run, &step, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.record_attempt(
        &run,
        &step,
        &Attempt {
            id: attempt.clone(),
            started_at: Timestamp(10),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Succeeded,
        },
    )
    .await
    .unwrap();
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let stream = scarab_engine::LogStreamId::new(&run, &step, &attempt);
    // The stream's tail lease, expired at once: its term still fences the
    // appends below (nobody took it over), and a reader does not take it
    // for a log still being stored.
    let resource = scarab_engine::LogLease::Tail.resource(&stream);
    let lease = db.lease(&resource, "w", 1).await.unwrap();
    let writer = scarab_engine::LogWriter::holding(
        scarab_engine::LogLease::Tail,
        &lease.held_by(&resource, "w").unwrap(),
    );
    logs.append(&run, &step, &attempt, b"one\r").await.unwrap();
    logs.append_marker(
        &stream,
        scarab_engine::LogRowKind::SourceRestarted,
        None,
        &writer,
        None,
        Timestamp(1),
    )
    .await
    .unwrap();
    logs.append(&run, &step, &attempt, b"\nmore\n")
        .await
        .unwrap();
    logs.append_marker(
        &stream,
        scarab_engine::LogRowKind::Incomplete,
        None,
        &writer,
        Some(scarab_engine::LogStreamState::Closed),
        Timestamp(2),
    )
    .await
    .unwrap();
    (
        router(AppState::new(
            db.clone(),
            Arc::new(FakeClock::new(1_000)),
            logs,
        )),
        db,
    )
}

/// 3.2/3.4: a marker row is its own `event: marker` with the next cursor
/// id; a resume from the marker's id sends only what follows; the `\n`
/// after a marker is not taken as the end of the `\r\n` before it. The raw
/// download writes `[scarab] <text>` lines and counts the markers. Kills: a
/// NULL `object_key` panic; markers sent as output; the `\r\n` join across
/// a marker.
#[tokio::test]
async fn resume_after_a_marker_and_raw_download_with_markers() {
    let (app, _db) = app_with_markers().await;
    let (status, body) = get(&app, "/v1/runs/r1/steps/build/logs", None).await;
    assert_eq!(status, StatusCode::OK);
    let evs = events(&body);
    let kinds: Vec<_> = evs
        .iter()
        .map(|e| e.event.clone().unwrap_or_default())
        .collect();
    assert_eq!(kinds, ["", "marker", "", "marker"], "{evs:?}");
    let marker: serde_json::Value = serde_json::from_str(&evs[1].data).unwrap();
    assert_eq!(marker["kind"], "source_restarted");
    assert!(marker["text"].as_str().unwrap().contains("restarted"));
    assert_eq!(evs[1].id.as_deref(), Some("a1:2"));
    // `\r` then, after the marker, a `\n` that starts a line of its own.
    assert_eq!(evs[2].data, "\nmore\n");
    // Resume from the marker's id: the rest only.
    let (_, rest) = get(&app, "/v1/runs/r1/steps/build/logs?from=a1:2", None).await;
    let rest = events(&rest);
    assert_eq!(rest.len(), 2, "{rest:?}");
    assert_eq!(rest[1].event.as_deref(), Some("marker"));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/steps/build/logs/raw?attempt=a1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("x-scarab-log-markers")
            .map(|v| v.to_str().unwrap()),
        Some("2")
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(bytes.to_vec()).unwrap(),
        "one\r\n[scarab] the log source restarted; lines before this point may be missing or \
         repeat\n\nmore\n\n[scarab] the end of this log could not be read\n"
    );
}

/// Q2 = C: `?markers=none` returns the stored output bytes exactly, and
/// still counts the markers in the header; another value is a 400. Kills:
/// marker lines in `?markers=none`.
#[tokio::test]
async fn raw_download_without_markers_is_byte_exact() {
    let (app, _db) = app_with_markers().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/steps/build/logs/raw?attempt=a1&markers=none")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-scarab-log-markers")
            .map(|v| v.to_str().unwrap()),
        Some("2")
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"one\r\nmore\n");
    let (status, _) = get(
        &app,
        "/v1/runs/r1/steps/build/logs/raw?attempt=a1&markers=all",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
