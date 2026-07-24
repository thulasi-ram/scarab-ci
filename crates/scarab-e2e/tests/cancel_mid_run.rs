//! Scenario 3 — **cancel mid-run tears down the Pod.**
//!
//! Start a deliberately slow run, `POST /v1/runs/{id}/cancel`, and assert BOTH
//! halves of the contract: the run's durable terminal status is `cancelled`
//! AND the step Pod actually disappears from the kind namespace (checked via
//! kubectl on the harness's isolated kubeconfig — a cancel that leaves a
//! zombie Pod is a lie).

mod support;

use std::time::{Duration, Instant};

use support::*;

#[tokio::test(flavor = "multi_thread")]
async fn cancel_mid_run_tears_down_the_pod() {
    require_e2e!();

    let base = base_url();
    let http = client();

    let run = create_inline_run(
        &http,
        &base,
        serde_json::json!([
            { "id": "slow", "image": "busybox:latest",
              "security": { "run_as_root": true },
              "command": ["sh", "-c", "sleep 600"] }
        ]),
    )
    .await;

    // Mid-run means: the step is running and its Pod exists on the cluster.
    wait_for_run(
        &http,
        &base,
        &run,
        Duration::from_secs(120),
        "step `slow` running",
        |rs| rs.step("slow").status == "running",
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while pods_of_run(&run).is_empty() {
        assert!(
            Instant::now() < deadline,
            "run {run}: step `slow` never got a Pod"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Cancel.
    let resp = http
        .post(format!("{base}/v1/runs/{run}/cancel"))
        .send()
        .await
        .expect("POST cancel");
    assert_eq!(resp.status().as_u16(), 202, "cancel must be accepted");

    // Durable half: the run terminates as `cancelled`, nothing else.
    let rs = wait_for_terminal(&http, &base, &run, Duration::from_secs(120)).await;
    assert_eq!(rs.status, "cancelled", "expected `cancelled`, got `{}`", rs.status);

    // Cluster half: the Pod disappears. Graceful deletion (SIGTERM → grace →
    // SIGKILL) means this is legitimately not instant — but it must converge.
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        let pods = pods_of_run(&run);
        if pods.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "run {run}: step Pod(s) still on the cluster after cancel: {pods:?}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
