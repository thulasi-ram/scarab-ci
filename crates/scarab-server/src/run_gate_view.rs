//! The Run page's **gate view** (slice b, criterion 5): what
//! `GET /v1/runs/{id}` adds to each gate step so the UI can say why it is
//! where it is.
//!
//! - **`gate_resolution`** — how the gate's latest wait resolved, read from its
//!   latest `GateResolved` in the run's journal. Never re-folded: the verdict
//!   is the one the engine recorded.
//!
//! Cheap by construction: the journal is read only when the run has a gate.

use std::collections::HashMap;

use scarab_engine::{AttemptOutcome, EventPayload, RunId, StepRun, StepStatus};

use crate::{ApiError, AppState, GateResolutionDto};

/// The gate facts of one step. Every field is `None` for a step with nothing
/// to say.
#[derive(Default)]
pub(crate) struct StepGateView {
    pub gate_resolution: Option<GateResolutionDto>,
}

/// A gate step whose wait is open: `Pending`, every need `Succeeded`, and its
/// latest Attempt still `Running` (the engine's "waiting gate").
fn is_waiting_gate(s: &StepRun, status_of: &HashMap<&str, StepStatus>) -> bool {
    s.is_gate()
        && s.status == StepStatus::Pending
        && needs_met(s, status_of)
        && s.attempts.last().map(|a| a.outcome) == Some(AttemptOutcome::Running)
}

/// The waiting gates of a run's `steps` ([`is_waiting_gate`]), in step order
/// — what the run list says a Run waits on (git-bug `c489346`).
pub(crate) fn waiting_gates(steps: &[StepRun]) -> Vec<&StepRun> {
    let status_of: HashMap<&str, StepStatus> = steps
        .iter()
        .map(|s| (s.step.0.as_str(), s.status))
        .collect();
    steps
        .iter()
        .filter(|s| is_waiting_gate(s, &status_of))
        .collect()
}

fn needs_met(s: &StepRun, status_of: &HashMap<&str, StepStatus>) -> bool {
    s.needs
        .iter()
        .all(|n| status_of.get(n.0.as_str()) == Some(&StepStatus::Succeeded))
}

/// The gate view of every step of `run` that has one, by step id.
pub(crate) async fn step_gate_views(
    st: &AppState,
    run: &RunId,
    steps: &[StepRun],
) -> Result<HashMap<String, StepGateView>, ApiError> {
    let mut views: HashMap<String, StepGateView> = HashMap::new();
    if !steps.iter().any(StepRun::is_gate) {
        return Ok(views);
    }
    for e in &st.db.events(run).await? {
        // `deployment` (ADR-0070) is only ever set on events from before
        // ADR-0074; the view does not show it.
        if let EventPayload::GateResolved {
            step,
            attempt,
            passed,
            reason,
            by,
            ..
        } = &e.kind
        {
            // The journal is in append order: the last one is the latest.
            views.entry(step.0.clone()).or_default().gate_resolution = Some(GateResolutionDto {
                passed: *passed,
                reason: reason.clone(),
                at: e.at.0,
                attempt: attempt.0.clone(),
                by: by.clone(),
            });
        }
    }
    Ok(views)
}
