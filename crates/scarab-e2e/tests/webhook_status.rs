//! Scenario 5 — webhook → run → status posted back (STUB).
//!
//! Planned shape (test-strategy Phase 2, scenario 5): stand up a fake forge
//! (as `happy_path` does), deliver a signed push webhook to
//! `/webhooks/forgejo`, assert a run is created and executes, and assert the
//! fake forge received the commit-status callbacks (pending → success).
//! The six scenarios are a cap, not a floor — this one is deliberately a
//! visible TODO rather than a rushed implementation.

mod support;

#[tokio::test]
#[ignore = "todo: scenario 5 — signed webhook ingest → run → status posted back to the fake forge"]
async fn webhook_creates_run_and_posts_status_back() {
    require_e2e!();
    unimplemented!("scenario 5 not yet implemented");
}
