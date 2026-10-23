//! The **DeploymentGate oracle** (slice b, criterion 2; ADR-0070 §8): the
//! server's implementation of [`scarab_engine::DeploymentGates`], which the
//! converged driver wires into the scheduler.
//!
//! A waiting `gate: deployment` step is resolved by **core**, from the fold of
//! its Environment's Deployment input log — never by a plugin. For one gate:
//!
//! 1. its Run's Project (the deploy context) and the deadline its
//!    DeploymentGates use: the Project's `EffectiveProtection`
//!    `deployment_gate_deadline` (the shorter of the Environment's baseline
//!    deadline and the Project's overlay), or the default deadline for an
//!    Environment the Org no longer has;
//! 2. the Environment's log, folded ([`scarab_deployment::fold_ledger`]);
//! 3. the Deployment the requester's current request landed on
//!    ([`DeploymentLedger::request_of`](scarab_deployment::DeploymentLedger::request_of)),
//!    and its verdict with the gate Attempt's start as the window floor
//!    ([`Deployment::gate_decision_from`](scarab_deployment::Deployment::gate_decision_from)).
//!
//! **Per tick.** Both reads are cached per tick ([`ServerDeploymentGates::for_tick`]):
//! every waiting gate on one Environment shares one fold, instead of each
//! refolding the whole log (ADR-0070 Consequences). A cached fold answers only
//! **as of when it was read**: the `now` it is asked with is lowered to the
//! `now` of the read that filled it, so an input committed after the read can
//! delay a verdict to the next tick but never be decided past. For the same
//! reason a window that opened after the read has no verdict yet.
//!
//! **Errors** are logged here and returned, and the engine takes no decision
//! that tick (plan Q6): one unreadable row must not dead-letter every Deploy
//! Run waiting in its Environment. `gate_expires_after` still bounds the wait.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use scarab_deployment::{
    fold_ledger, DeploymentGateFailure, DeploymentGateVerdict, DeploymentRequester,
    DeploymentStore, DeploymentTarget, FoldedLedger, Millis, DEFAULT_DEPLOYMENT_DEADLINE_MS,
};
use scarab_engine::{
    Db, DeploymentGateDecision, DeploymentGates, RunDeploymentRequest, RunId, StepId, Timestamp,
};
use scarab_project::EnvironmentGovernanceStore;

/// One Environment's log, folded, and the `now` it may answer for.
struct CachedFold {
    folded: FoldedLedger,
    read_now: Millis,
}

/// The server's DeploymentGate oracle. Construct one per process with
/// [`ServerDeploymentGates::new`]; the driver takes a fresh
/// [`for_tick`](Self::for_tick) view each tick so the fold cache lives for one
/// tick only.
pub struct ServerDeploymentGates {
    db: Arc<dyn Db>,
    store: Arc<dyn DeploymentStore>,
    /// `None`: no org-level governance; every gate uses the default deadline.
    governance: Option<Arc<dyn EnvironmentGovernanceStore>>,
    folds: Mutex<HashMap<(String, String), Arc<CachedFold>>>,
    /// The gate deadline, by (org, project, environment).
    deadlines: Mutex<HashMap<(String, String, String), Millis>>,
}

impl ServerDeploymentGates {
    pub fn new(
        db: Arc<dyn Db>,
        store: Arc<dyn DeploymentStore>,
        governance: Option<Arc<dyn EnvironmentGovernanceStore>>,
    ) -> Self {
        Self {
            db,
            store,
            governance,
            folds: Mutex::new(HashMap::new()),
            deadlines: Mutex::new(HashMap::new()),
        }
    }

    /// The same oracle with empty caches: what one tick asks.
    pub fn for_tick(&self) -> Self {
        Self::new(self.db.clone(), self.store.clone(), self.governance.clone())
    }

    /// The Environment's fold, read (and cached for this tick) at `now`.
    async fn fold(
        &self,
        org: &str,
        environment: &str,
        now: Millis,
    ) -> Result<Arc<CachedFold>, String> {
        let key = (org.to_string(), environment.to_string());
        if let Some(cached) = self.folds.lock().expect("fold cache poisoned").get(&key) {
            return Ok(cached.clone());
        }
        let inputs = self
            .store
            .inputs(org, environment)
            .await
            .map_err(|e| e.to_string())?;
        let cached = Arc::new(CachedFold {
            folded: fold_ledger(&inputs),
            read_now: now,
        });
        self.folds
            .lock()
            .expect("fold cache poisoned")
            .insert(key, cached.clone());
        Ok(cached)
    }

    /// The deadline `project`'s DeploymentGates use in `environment`.
    async fn gate_deadline(
        &self,
        org: &str,
        project: &str,
        environment: &str,
    ) -> Result<Millis, String> {
        let key = (
            org.to_string(),
            project.to_string(),
            environment.to_string(),
        );
        if let Some(d) = self
            .deadlines
            .lock()
            .expect("deadline cache poisoned")
            .get(&key)
        {
            return Ok(*d);
        }
        let deadline = match &self.governance {
            None => DEFAULT_DEPLOYMENT_DEADLINE_MS,
            Some(store) => store
                .deploy_governance(org, project, environment)
                .await
                .map_err(|e| format!("environment governance store: {e}"))?
                // An Environment the Org no longer has: the default, as the
                // Deployments view folds such a name (ADR-0071).
                .map_or(DEFAULT_DEPLOYMENT_DEADLINE_MS, |g| {
                    g.effective_protection().deployment_gate_deadline.millis()
                }),
        };
        self.deadlines
            .lock()
            .expect("deadline cache poisoned")
            .insert(key, deadline);
        Ok(deadline)
    }

    async fn decide(
        &self,
        run: &RunId,
        request: &RunDeploymentRequest,
        window_floor: Timestamp,
        now: Timestamp,
    ) -> Result<Option<DeploymentGateDecision>, String> {
        let deadline = match self
            .db
            .run_deploy_context(run)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(ctx) => {
                self.gate_deadline(&request.org, &ctx.project, &request.environment)
                    .await?
            }
            None => {
                // A Run with a DeploymentRequest is a Deploy Run, which has a
                // deploy context; without one there is no overlay to read.
                tracing::warn!(
                    run = %run.0,
                    "deployment gate on a run with no deploy context; using the default deadline"
                );
                DEFAULT_DEPLOYMENT_DEADLINE_MS
            }
        };
        let cached = self.fold(&request.org, &request.environment, now.0).await?;
        let now = now.0.min(cached.read_now);
        if now < window_floor.0 {
            // The fold predates the window: it cannot speak for it yet.
            return Ok(None);
        }
        let target = DeploymentTarget {
            environment: request.environment.clone(),
            source: request.target_source.clone(),
            id: request.target_id.clone(),
        };
        let requester = DeploymentRequester {
            run: run.0.clone(),
            step: request.step.0.clone(),
        };
        let ledger = cached.folded.ledger();
        let Some(deployment) = ledger.request_of(&requester, &target) else {
            // The requesting step succeeded before this gate's wait began,
            // and its request was recorded in the same commit as that
            // success — and this fold was read after the wait began. So the
            // request is not late: it is missing, and waiting cannot bring it.
            return Ok(Some(DeploymentGateDecision {
                passed: false,
                reason: format!(
                    "no DeploymentRequest from step `{}` for `{}`/`{}` is in the Deployment \
                     record",
                    request.step.0, request.target_source, request.target_id
                ),
                deployment: None,
            }));
        };
        let Some(decision) =
            deployment.gate_decision_from(&requester, window_floor.0, now, deadline)
        else {
            return Ok(None);
        };
        let on = format!("`{}`/`{}`", request.target_source, request.target_id);
        let version = &deployment.version.0;
        let (passed, reason) = match decision.verdict {
            DeploymentGateVerdict::Passed { .. } => (
                true,
                match decision.report {
                    Some(r) if !r.label.trim().is_empty() => {
                        format!("{version} live on {on}: {}", r.label)
                    }
                    Some(_) => format!("{version} live on {on}"),
                    None => format!("{version} was already live on {on}"),
                },
            ),
            DeploymentGateVerdict::Failed {
                reason: DeploymentGateFailure::Failed,
                ..
            } => (
                false,
                // The source's label is the reason (ADR-0070 §8).
                match decision.report {
                    Some(r) if !r.label.trim().is_empty() => r.label.clone(),
                    _ => format!("{version} reported failed on {on}"),
                },
            ),
            DeploymentGateVerdict::Failed {
                reason: DeploymentGateFailure::Superseded,
                ..
            } => (
                false,
                match deployment.superseded_by().and_then(|id| ledger.get(id)) {
                    Some(newer) => format!("superseded by {}", newer.version.0),
                    None => "superseded".to_string(),
                },
            ),
            DeploymentGateVerdict::Failed {
                reason: DeploymentGateFailure::Stalled,
                ..
            } => (
                false,
                format!("stalled: not live within {}", deadline_text(deadline)),
            ),
        };
        Ok(Some(DeploymentGateDecision {
            passed,
            reason,
            deployment: Some(cached.folded.public_id(deployment)),
        }))
    }
}

/// A deadline as a reason shows it: whole seconds when it is a whole number
/// of them, else milliseconds.
fn deadline_text(ms: Millis) -> String {
    if ms % 1000 == 0 {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

#[async_trait]
impl DeploymentGates for ServerDeploymentGates {
    async fn deployment_gate_verdict(
        &self,
        run: &RunId,
        gate: &StepId,
        request: &RunDeploymentRequest,
        window_floor: Timestamp,
        now: Timestamp,
    ) -> Result<Option<DeploymentGateDecision>, String> {
        let res = self.decide(run, request, window_floor, now).await;
        if let Err(e) = &res {
            tracing::warn!(
                run = %run.0, gate = %gate.0, error = %e,
                "deployment gate: no decision this tick; asked again next tick"
            );
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::deadline_text;

    #[test]
    fn a_deadline_reads_in_seconds_when_whole() {
        assert_eq!(deadline_text(3_600_000), "3600s");
        assert_eq!(deadline_text(1_500), "1500ms");
    }
}
