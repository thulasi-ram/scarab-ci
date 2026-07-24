//! Scenario 4 — **rerun produces a new Take with correct attempt evidence.**
//!
//! Run a pipeline to success, rerun a step, and assert the ADR-0056 shape via
//! the public API: a `RunRerunRequested` event marks the Take boundary (with
//! the pressed step as target), a SECOND `AttemptStarted` with a distinct
//! attempt id records the re-execution, and the run settles `succeeded` again
//! with the step's of-record attempt count updated to 2.

mod support;

use std::time::Duration;

use support::*;

#[tokio::test(flavor = "multi_thread")]
async fn rerun_creates_a_new_take_with_fresh_attempt_evidence() {
    require_e2e!();

    let base = base_url();
    let http = client();

    let run = create_inline_run(
        &http,
        &base,
        serde_json::json!([
            { "id": "hello", "image": "busybox:latest",
              "security": { "run_as_root": true },
              "command": ["sh", "-c", "echo take output"] }
        ]),
    )
    .await;

    let rs = wait_for_terminal(&http, &base, &run, Duration::from_secs(180)).await;
    assert_eq!(rs.status, "succeeded", "baseline run must succeed first");
    assert_eq!(rs.step("hello").attempts, 1);

    // Rerun the step — the human Take boundary.
    let resp = http
        .post(format!("{base}/v1/runs/{run}/steps/hello/rerun"))
        .send()
        .await
        .expect("POST rerun");
    assert_eq!(resp.status().as_u16(), 202, "rerun must be accepted");

    // The run leaves its terminal state, re-executes, and settles again with
    // TWO attempts of record on the step.
    let rs = wait_for_run(
        &http,
        &base,
        &run,
        Duration::from_secs(180),
        "a second succeeded take (2 attempts)",
        |rs| rs.status == "succeeded" && rs.step("hello").attempts == 2,
    )
    .await;
    assert_eq!(rs.step("hello").attempts, 2);

    // Event evidence: exactly one Take boundary, targeting the pressed step…
    let evs = events(&http, &base, &run).await;
    let reruns = events_of_kind(&evs, "RunRerunRequested");
    assert_eq!(
        reruns.len(),
        1,
        "expected exactly one RunRerunRequested, got {reruns:?}"
    );
    assert_eq!(reruns[0]["target"], "hello");
    assert!(
        reruns[0]["invalidated"]
            .as_array()
            .is_some_and(|inv| inv.iter().any(|s| s == "hello")),
        "the invalidation set must contain the target: {:?}",
        reruns[0]["invalidated"]
    );

    // …and two distinct executions of the step (fresh attempt id in take 2).
    let started: Vec<_> = events_of_kind(&evs, "AttemptStarted")
        .into_iter()
        .filter(|p| p["step"] == "hello")
        .collect();
    assert_eq!(
        started.len(),
        2,
        "expected two AttemptStarted for `hello`, got {started:?}"
    );
    assert_ne!(
        started[0]["attempt"], started[1]["attempt"],
        "the rerun must mint a NEW attempt, not reuse the old id"
    );

    // The of-record output is fresh: the log carries the step output from the
    // re-execution too (both attempts' logs replay).
    let text = logs(&http, &base, &run).await;
    let occurrences = text.matches("take output").count();
    assert!(
        occurrences >= 2,
        "expected the rerun attempt's output in the logs (>=2 occurrences), got {occurrences}: {text:?}"
    );
}
