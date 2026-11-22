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

/// git-bug 9b795be: a rerun whose step writes the same bytes again skips the
/// step downstream of it. `a` writes a fixed file into the workspace and `b`
/// reads it; nothing in `a` depends on the time or a random value. Rerunning
/// `a` runs it again in a new Pod; its content identity matches the first
/// Attempt's, so `b`'s inputs are unchanged and `b` is skipped with its
/// output carried forward (ADR-0027). Before content identity (ADR-0061 s8)
/// two Pods of one step always differed (mtime, drain order), so `b` always
/// ran again; the in-memory executor cannot show that difference.
#[tokio::test(flavor = "multi_thread")]
async fn a_rerun_with_identical_output_skips_the_downstream_step() {
    require_e2e!();

    let base = base_url();
    let http = client();

    let run = create_inline_run(
        &http,
        &base,
        serde_json::json!([
            { "id": "a", "image": "busybox:latest",
              "security": { "run_as_root": true },
              "command": ["sh", "-c", "printf fixed > /workspace/out"] },
            { "id": "b", "image": "busybox:latest", "needs": ["a"],
              "security": { "run_as_root": true },
              "command": ["sh", "-c", "cat /workspace/out"] }
        ]),
    )
    .await;

    let rs = wait_for_terminal(&http, &base, &run, Duration::from_secs(180)).await;
    assert_eq!(rs.status, "succeeded", "baseline run must succeed first");
    assert_eq!((rs.step("a").attempts, rs.step("b").attempts), (1, 1));

    let resp = http
        .post(format!("{base}/v1/runs/{run}/steps/a/rerun"))
        .send()
        .await
        .expect("POST rerun");
    assert_eq!(resp.status().as_u16(), 202, "rerun must be accepted");

    // The Run is still `succeeded` right after the 202, so the attempt count
    // is part of what is waited for.
    wait_for_run(
        &http,
        &base,
        &run,
        Duration::from_secs(180),
        "succeeded again with a second attempt of `a`",
        |rs| rs.status == "succeeded" && rs.step("a").attempts == 2,
    )
    .await;

    let evs = events(&http, &base, &run).await;
    let kinds: Vec<&str> = evs.iter().map(event_kind).collect();
    let rerun_at = kinds
        .iter()
        .position(|k| *k == "RunRerunRequested")
        .unwrap_or_else(|| panic!("no RunRerunRequested event: {kinds:?}"));
    // Positions in the event log, which is in append order.
    let skipped: Vec<usize> = evs
        .iter()
        .enumerate()
        .filter(|(_, e)| event_kind(e) == "StepSkipped" && e["kind"]["StepSkipped"]["step"] == "b")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        skipped.len(),
        1,
        "expected exactly one StepSkipped for `b` (its input is unchanged): {kinds:?}"
    );
    assert!(
        skipped[0] > rerun_at,
        "the StepSkipped for `b` must follow the RunRerunRequested: {kinds:?}"
    );

    let started = events_of_kind(&evs, "AttemptStarted");
    let of = |step: &str| -> Vec<&serde_json::Value> {
        started
            .iter()
            .copied()
            .filter(|p| p["step"] == step)
            .collect()
    };
    assert_eq!(
        of("b").len(),
        1,
        "`b` ran once, in the first Take only: {:?}",
        of("b")
    );
    let a = of("a");
    assert_eq!(a.len(), 2, "`a` ran twice: {a:?}");
    assert_ne!(
        a[0]["attempt"], a[1]["attempt"],
        "the rerun of `a` is a new attempt"
    );
}
