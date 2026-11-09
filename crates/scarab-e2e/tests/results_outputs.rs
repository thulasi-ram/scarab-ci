//! A step's results reach a later step through `${{ outputs.<step>.<name> }}`
//! (ADR-0041, ADR-0042), on the kind release in its e2e mode.
//!
//! The first step writes `/scarab/results/<name>.json`; the egress sidecar
//! drains those files; the second step's env is interpolated from them at
//! launch. The second step checks the values itself and exits non-zero on a
//! mismatch, so a succeeded Run is the proof. This is the live check git-bug
//! 8de85a8 waited on: the dogfood `version` → `deploy` chain, without the
//! dogfood stack.

mod support;

use std::time::Duration;

use support::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_result_is_interpolated_into_a_later_step() {
    require_e2e!();

    let base = base_url();
    let client = client();

    // busybox runs as root, so each step takes the self-service run_as_root
    // grant; the hardened baseline refuses the image otherwise.
    let run = create_inline_run(
        &client,
        &base,
        serde_json::json!([
            { "id": "version", "image": "busybox:latest",
              "security": { "run_as_root": true },
              "results": ["tag", "commits"],
              "command": ["sh", "-c",
                "printf '\"v1.2.3\"' > /scarab/results/tag.json && \
                 printf '42' > /scarab/results/commits.json"] },
            { "id": "deploy", "image": "busybox:latest", "needs": ["version"],
              "security": { "run_as_root": true },
              "env": {
                "PREV_VERSION": "${{ outputs.version.tag }}",
                "COMMIT_COUNT": "${{ outputs.version.commits }}"
              },
              "command": ["sh", "-c",
                "set -eu; echo \"tag=$PREV_VERSION commits=$COMMIT_COUNT\"; \
                 test \"$PREV_VERSION\" = v1.2.3; test \"$COMMIT_COUNT\" = 42"] }
        ]),
    )
    .await;

    let rs = wait_for_terminal(&client, &base, &run, Duration::from_secs(240)).await;
    assert_eq!(
        rs.status,
        "succeeded",
        "deploy must see version's results in its env; steps: {:?}",
        rs.steps
            .iter()
            .map(|s| format!("{}:{}", s.id, s.status))
            .collect::<Vec<_>>()
    );
}
