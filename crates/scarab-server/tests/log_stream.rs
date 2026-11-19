//! Log streaming over HTTP (ADR-0013): batched replay with cursor ids, resume
//! from `?from=` / `Last-Event-ID`, the `?tail=` cut, UTF-8 characters split
//! across chunks, and the raw download. Hermetic: InMemoryDb + an in-memory
//! object store.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{Attempt, AttemptId, AttemptOutcome, Db, RunId, RunStatus, StepId, Timestamp};
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

/// Split an SSE body into events (LF-only, as axum writes it; the test logs
/// hold no CR).
fn events(body: &str) -> Vec<Ev> {
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
