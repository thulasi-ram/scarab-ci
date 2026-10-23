//! The Run page's **gate and hold view** (slice b, criterion 5): what
//! `GET /v1/runs/{id}` adds to each step so the UI can say why a gate or a
//! deploy Step is where it is.
//!
//! Three facts, each from the source that owns it:
//!
//! - **`gate_resolution`** — how the gate's latest wait resolved, read from its
//!   latest `GateResolved` in the run's journal. Never re-folded: the verdict
//!   is the one the engine recorded.
//! - **`deployment_gate`** — for a DeploymentGate that is waiting right now,
//!   the DeploymentTarget it waits on and where the Deployment its requester
//!   asked for stands: the Environment's log folded as the Deployments page
//!   folds it (the Environment's own deadline). Display only; the verdict is
//!   the engine's, from its own fold.
//! - **`held`** — why a deploy Step is held by per-DeploymentTarget
//!   concurrency, asked of the same oracle the scheduler asks
//!   ([`ServerDeploymentGates::step_concurrency_hold`]). Derived, because
//!   nothing is journalled per scheduler pass. Never for a queued (`Pending`)
//!   Run, which the scheduler has not yet asked about.
//!
//! Cheap by construction: the journal is read only when the run has a gate,
//! request rows only when it has a waiting DeploymentGate or a Step that could
//! be held, and each Environment's log once per request. A read error in the
//! two derived facts is logged and degrades the view; it never fails the page.

use std::collections::HashMap;
use std::sync::Arc;

use scarab_deployment::{fold_ledger, DeploymentRequester, DeploymentTarget, FoldedLedger};
use scarab_engine::{
    AttemptOutcome, EventPayload, RunDeploymentRequest, RunId, RunStatus, StepRun, StepStatus,
};

use crate::deployment_gates::ServerDeploymentGates;
use crate::{org_environments, ApiError, AppState, DeploymentGateDto, GateResolutionDto};

/// The gate/hold facts of one step. Every field is `None` for a step with
/// nothing to say.
#[derive(Default)]
pub(crate) struct StepGateView {
    pub gate_resolution: Option<GateResolutionDto>,
    pub deployment_gate: Option<DeploymentGateDto>,
    pub held: Option<String>,
}

/// A gate step whose wait is open: `Pending`, every need `Succeeded`, and its
/// latest Attempt still `Running` (the engine's "waiting gate").
fn is_waiting_gate(s: &StepRun, status_of: &HashMap<&str, StepStatus>) -> bool {
    s.is_gate()
        && s.status == StepStatus::Pending
        && needs_met(s, status_of)
        && s.attempts.last().map(|a| a.outcome) == Some(AttemptOutcome::Running)
}

fn needs_met(s: &StepRun, status_of: &HashMap<&str, StepStatus>) -> bool {
    s.needs
        .iter()
        .all(|n| status_of.get(n.0.as_str()) == Some(&StepStatus::Succeeded))
}

/// The gate/hold view of every step of `run` that has one, by step id.
pub(crate) async fn step_gate_views(
    st: &AppState,
    run: &RunId,
    run_status: RunStatus,
    steps: &[StepRun],
) -> Result<HashMap<String, StepGateView>, ApiError> {
    let mut views: HashMap<String, StepGateView> = HashMap::new();
    if steps.iter().any(StepRun::is_gate) {
        for e in st.db.events(run).await? {
            if let EventPayload::GateResolved {
                step,
                attempt,
                passed,
                reason,
                deployment,
                by,
            } = e.kind
            {
                // The journal is in append order: the last one is the latest.
                views.entry(step.0).or_default().gate_resolution = Some(GateResolutionDto {
                    passed,
                    reason,
                    at: e.at.0,
                    attempt: attempt.0,
                    by,
                    deployment_id: deployment,
                });
            }
        }
    }

    // A terminal run waits on nothing and holds nothing.
    if !matches!(
        run_status,
        RunStatus::Pending | RunStatus::Running | RunStatus::Suspended
    ) {
        return Ok(views);
    }
    let status_of: HashMap<&str, StepStatus> = steps
        .iter()
        .map(|s| (s.step.0.as_str(), s.status))
        .collect();

    let waiting: Vec<&StepRun> = steps
        .iter()
        .filter(|s| s.gate_kind.as_deref() == Some("deployment") && is_waiting_gate(s, &status_of))
        .collect();
    if !waiting.is_empty() {
        let mut requests: Vec<RunDeploymentRequest> = Vec::new();
        for s in steps.iter().filter(|s| !s.is_gate()) {
            requests.extend(st.db.run_deployment_requests(run, &s.step).await?);
        }
        let mut folds: HashMap<(String, String), Option<Arc<FoldedLedger>>> = HashMap::new();
        let now = st.clock.now().await.0;
        for gate in waiting {
            // No row: the engine fails such a gate at once; nothing to show.
            let Some(request) = requests.iter().find(|r| r.gate_steps.contains(&gate.step)) else {
                continue;
            };
            let dto = deployment_gate_dto(st, run, request, &mut folds, now).await;
            views
                .entry(gate.step.0.clone())
                .or_default()
                .deployment_gate = Some(dto);
        }
    }

    // A deploy Step the scheduler would ask about: Pending, needs met. The
    // oracle answers `None` for one that makes no DeploymentRequest. Never
    // for a queued (`Pending`) Run: the scheduler asks only once the Run has
    // its run-level slot, so "held: deployment concurrency" could name the
    // wrong reason for a Run that waits on its concurrency group or a cap.
    if run_status == RunStatus::Pending {
        return Ok(views);
    }
    if let Some(store) = st.deployments.clone() {
        let candidates: Vec<&StepRun> = steps
            .iter()
            .filter(|s| !s.is_gate() && s.status == StepStatus::Pending && needs_met(s, &status_of))
            .collect();
        if !candidates.is_empty() {
            let oracle =
                ServerDeploymentGates::new(st.db.clone(), store, st.environment_governance.clone());
            let now = st.clock.now().await;
            for s in candidates {
                let held = match oracle.step_concurrency_hold(run, &s.step, now).await {
                    Ok(hold) => hold.map(|h| h.reason()),
                    // The scheduler holds an undecidable Step (fail closed);
                    // say so rather than show it as merely pending.
                    Err(e) => {
                        tracing::warn!(run = %run.0, step = %s.step.0, error = %e,
                            "run view: deployment concurrency undecidable");
                        Some(format!(
                            "held: deployment concurrency could not be decided: {e}"
                        ))
                    }
                };
                if held.is_some() {
                    views.entry(s.step.0.clone()).or_default().held = held;
                }
            }
        }
    }
    Ok(views)
}

/// Where a waiting DeploymentGate's request stands, as the Deployments page
/// would show it. Without a Deployment store only the target is known; on a
/// read error (logged) only the target, and `record_unreadable` says why.
async fn deployment_gate_dto(
    st: &AppState,
    run: &RunId,
    request: &RunDeploymentRequest,
    folds: &mut HashMap<(String, String), Option<Arc<FoldedLedger>>>,
    now: scarab_deployment::Millis,
) -> DeploymentGateDto {
    let mut dto = DeploymentGateDto {
        environment: request.environment.clone(),
        target_source: request.target_source.clone(),
        target_id: request.target_id.clone(),
        deployment_id: None,
        deployment_status: None,
        version: None,
        latest_label: None,
        record_unreadable: false,
    };
    let Some(store) = st.deployments.as_ref() else {
        return dto;
    };
    let key = (request.org.clone(), request.environment.clone());
    let folded = match folds.get(&key) {
        Some(f) => f.clone(),
        None => {
            let f = match store.inputs(&request.org, &request.environment).await {
                Ok(inputs) => Some(Arc::new(fold_ledger(&inputs))),
                Err(e) => {
                    tracing::warn!(run = %run.0, error = %e,
                        "run view: could not read the Deployment record");
                    None
                }
            };
            folds.insert(key, f.clone());
            f
        }
    };
    let Some(folded) = folded else {
        // The read failed (logged above): say so, rather than let the view
        // read as "no Deployment recorded yet", which would be false.
        dto.record_unreadable = true;
        return dto;
    };
    let target = DeploymentTarget {
        environment: request.environment.clone(),
        source: request.target_source.clone(),
        id: request.target_id.clone(),
    };
    let requester = DeploymentRequester {
        run: run.0.clone(),
        step: request.step.0.clone(),
    };
    let Some(deployment) = folded.ledger().request_of(&requester, &target) else {
        return dto;
    };
    let deadline = match org_environments::deadline_from_org_environment(
        st,
        &request.org,
        &request.environment,
    )
    .await
    {
        Ok(d) => d,
        Err(_) => scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS,
    };
    dto.deployment_id = Some(folded.public_id(deployment));
    dto.deployment_status = Some(deployment.status(now, deadline).into());
    dto.version = Some(deployment.version.0.clone());
    dto.latest_label = deployment
        .reports
        .last()
        .map(|r| r.label.clone())
        .filter(|l| !l.trim().is_empty());
    dto
}
