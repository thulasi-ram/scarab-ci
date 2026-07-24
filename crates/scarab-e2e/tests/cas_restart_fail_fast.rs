//! Scenario 6 — workspace/CAS restart fails fast (STUB).
//!
//! Planned shape (test-strategy Phase 2, scenario 6; the known helm-dogfood
//! escape): complete a run whose step produces a workspace, wipe the CAS
//! backing store (simulating the emptyDir loss on server restart), rerun a
//! step that consumes the workspace, and assert the run FAILS FAST with a
//! legible error instead of hanging at Pod Init and dead-lettering minutes
//! later. Write once `drive_workspace` grows the fail-fast (tracked in the
//! helm-dogfood follow-ups) — today this would pin the broken behaviour.

mod support;

#[tokio::test]
#[ignore = "todo: scenario 6 — CAS loss on restart must fail fast, not hang at Init (helm-dogfood escape)"]
async fn rerun_after_cas_loss_fails_fast() {
    require_e2e!();
    unimplemented!("scenario 6 not yet implemented");
}
