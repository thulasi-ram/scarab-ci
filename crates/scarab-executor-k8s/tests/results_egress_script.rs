//! The results-egress sidecar's entrypoint, `docker/sidecar/scarab-results-egress.sh`,
//! run with the host's `/bin/sh`, `curl` and `jq` against a listener on
//! localhost standing in for the server. Only the ingest's 202 counts as
//! delivered: a 200 with HTML (what the server's web UI fallback sent when a
//! raw `/` in a Step id made the URL match no API route) is a failure the
//! sidecar logs, retries, and exits non-zero on.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SCRIPT: &str = include_str!("../../../docker/sidecar/scarab-results-egress.sh");

/// Serve every request on a fresh localhost port with `reply` (a full HTTP
/// response), counting the requests. Returns the base URL and the count.
fn serve(reply: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            seen.fetch_add(1, Ordering::SeqCst);
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (base, count)
}

/// Run the script with one result in its results dir, posting to `url`;
/// SIGTERM it once it watches, as the kubelet does after the step exits.
/// Returns its exit code and stderr.
fn drain_to(url: &str) -> (Option<i32>, String) {
    let results = tempfile::tempdir().unwrap();
    std::fs::write(
        results.path().join("image.json"),
        r#"{"digest":"sha256:ab"}"#,
    )
    .unwrap();
    let mut child = Command::new("/bin/sh")
        .args(["-c", SCRIPT, "scarab-results-egress"])
        .env("SCARAB_RESULTS", results.path())
        .env("SCARAB_RESULTS_URL", url)
        .env("SCARAB_RESULTS_TOKEN", "token")
        .env("SCARAB_ATTEMPT", "a1")
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the sidecar script (needs /bin/sh, curl and jq)");
    // Let it reach its watch loop, then terminate it.
    std::thread::sleep(Duration::from_millis(500));
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the sidecar did not finish its drain"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status.code(), stderr)
}

#[test]
fn a_202_from_the_ingest_is_delivered() {
    let (base, count) =
        serve("HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    let (code, stderr) = drain_to(&format!("{base}/v1/runs/r/steps/amd64%2Fimage/results"));
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stderr.contains("results posted"), "{stderr}");
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn an_html_200_is_a_failed_delivery() {
    let (base, count) = serve(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\n\
         content-length: 15\r\nconnection: close\r\n\r\n<!doctype html>",
    );
    let (code, stderr) = drain_to(&format!("{base}/v1/runs/r/steps/amd64/image/results"));
    assert_eq!(code, Some(1), "{stderr}");
    assert!(!stderr.contains("results posted"), "{stderr}");
    assert!(stderr.contains("answered 200 text/html"), "{stderr}");
    assert!(stderr.contains("NOT delivered"), "{stderr}");
    assert_eq!(
        count.load(Ordering::SeqCst),
        6,
        "retried like any other failure"
    );
}

#[test]
fn an_error_answer_is_a_failed_delivery() {
    let (base, count) = serve(
        "HTTP/1.1 401 Unauthorized\r\ncontent-length: 23\r\nconnection: close\r\n\r\n\
         authentication required",
    );
    let (code, stderr) = drain_to(&format!("{base}/v1/runs/r/steps/build/results"));
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("answered 401"), "{stderr}");
    assert!(stderr.contains("authentication required"), "{stderr}");
    assert_eq!(count.load(Ordering::SeqCst), 6);
}
