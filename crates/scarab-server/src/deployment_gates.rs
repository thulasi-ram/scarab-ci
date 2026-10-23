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
//! reason a window that opened after the read has no verdict yet. The one
//! irreversible verdict a fold can give without any input — "the request is
//! missing" — is never taken from a cached fold: the record is re-read
//! uncached first (the cache entry replaced) and the gate fails only if that
//! fresh read still lacks the request. That is causal, not clock order:
//! the request commits with the deploy Step's success, which the gate's wait
//! opened after, so a read issued after the wait opened sees it.
//!
//! **Errors** are logged here and returned, and the engine takes no decision
//! that tick (plan Q6): one unreadable row must not dead-letter every Deploy
//! Run waiting in its Environment — but only until the deadline. A record
//! that still cannot be read once the deadline has elapsed since the window
//! start fails the gate `stalled`, the read error in the reason, so the
//! deadline bounds the wait even with no `gate_expires_after`. An error
//! reading the deadline itself (governance store, deploy context) stays no
//! decision.
//!
//! **Per-DeploymentTarget concurrency** (slice b, criterion 4; ADR-0071 §9)
//! is decided here too: a deploy Step about to be promoted is held `Pending`
//! while the Runs other than its own in flight on one of its targets
//! ([`Db::deployment_requesting_runs`]) number its Project's effective
//! `deployment_concurrency` in the Environment (the tighter of the baseline
//! and the Project's overlay — so the limit a Run sees is its own Project's;
//! the count is every Project's Runs on the target). The limit is cached per
//! tick like the deadline; the count never is — a Run admitted earlier in the
//! same tick must be counted. An error holds the Step (fail closed). A Run
//! counted only through DeploymentGates that are not yet reached stops
//! counting once the Environment's baseline deadline has elapsed since its
//! request's effective time — the moment its Deployment reads `stalled` on
//! the record — so opposite-order holds cannot deadlock two Runs (R3).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use scarab_deployment::{
    fold_ledger, DeploymentGateFailure, DeploymentGateVerdict, DeploymentRequester,
    DeploymentStore, DeploymentTarget, FoldedLedger, Millis, DEFAULT_DEPLOYMENT_DEADLINE_MS,
};
use scarab_engine::{
    Db, DeploymentConcurrencyHold, DeploymentGateDecision, DeploymentGates, RunDeploymentRequest,
    RunId, StepId, Timestamp,
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
    /// The Project's deploy rules in an Environment, by (org, project,
    /// environment).
    rules: Mutex<HashMap<(String, String, String), ProjectDeployRules>>,
}

/// What this oracle reads from a Project's `EffectiveProtection` in one
/// Environment.
#[derive(Debug, Clone, Copy)]
struct ProjectDeployRules {
    /// The deadline the Project's DeploymentGates use.
    gate_deadline: Millis,
    /// The Environment's **baseline** deadline — never shorter than any
    /// Project's `gate_deadline` there (tighten-only), and one value per
    /// Environment: what bounds an unreached gate's hold on a target (R3).
    baseline_deadline: Millis,
    /// Its per-DeploymentTarget concurrency limit; `None` = unlimited.
    deployment_concurrency: Option<std::num::NonZeroU32>,
}

impl ProjectDeployRules {
    /// No governance to read (none wired, or the Org no longer has the
    /// Environment): the default deadline and no concurrency limit — a limit
    /// is an explicit opt-in that only governance can express.
    const UNGOVERNED: Self = Self {
        gate_deadline: DEFAULT_DEPLOYMENT_DEADLINE_MS,
        baseline_deadline: DEFAULT_DEPLOYMENT_DEADLINE_MS,
        deployment_concurrency: None,
    };
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
            rules: Mutex::new(HashMap::new()),
        }
    }

    /// The same oracle with empty caches: what one tick asks.
    pub fn for_tick(&self) -> Self {
        Self::new(self.db.clone(), self.store.clone(), self.governance.clone())
    }

    /// The Environment's fold, read (and cached for this tick) at `now`.
    /// `true` with it when it was served from the cache — read EARLIER in
    /// this tick, so possibly before an input this caller needs committed.
    async fn fold(
        &self,
        org: &str,
        environment: &str,
        now: Millis,
    ) -> Result<(Arc<CachedFold>, bool), String> {
        let key = (org.to_string(), environment.to_string());
        if let Some(cached) = self.folds.lock().expect("fold cache poisoned").get(&key) {
            return Ok((cached.clone(), true));
        }
        Ok((self.read_fold(org, environment, now).await?, false))
    }

    /// The Environment's fold read NOW, uncached and below the store's
    /// horizon ([`DeploymentStore::inputs_as_of`]), replacing this tick's
    /// cache entry (so later gates of the tick share the newer read). The
    /// one place both the first read and the "missing request" re-read go
    /// through.
    async fn read_fold(
        &self,
        org: &str,
        environment: &str,
        now: Millis,
    ) -> Result<Arc<CachedFold>, String> {
        let key = (org.to_string(), environment.to_string());
        // Below the store's horizon (ADR-0070 §8): the read first fences the
        // Org's stamp clock past `now`, so every input it does not see is
        // stamped after `now` — the fold is complete for the instant this
        // gate decides for, and a verdict reached by absence (`stalled`) or
        // by the state before the window (`passed`) can never be contradicted
        // by an input stamped at or before it (R3).
        let (inputs, horizon) = self
            .store
            .inputs_as_of(org, environment, now)
            .await
            .map_err(|e| e.to_string())?;
        debug_assert!(horizon > now, "the fence stamps after now");
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

    /// `project`'s deploy rules in `environment`, cached for this tick.
    async fn rules(
        &self,
        org: &str,
        project: &str,
        environment: &str,
    ) -> Result<ProjectDeployRules, String> {
        let key = (
            org.to_string(),
            project.to_string(),
            environment.to_string(),
        );
        if let Some(r) = self.rules.lock().expect("rules cache poisoned").get(&key) {
            return Ok(*r);
        }
        let rules = match &self.governance {
            None => ProjectDeployRules::UNGOVERNED,
            Some(store) => store
                .deploy_governance(org, project, environment)
                .await
                .map_err(|e| format!("environment governance store: {e}"))?
                // An Environment the Org no longer has: the default, as the
                // Deployments view folds such a name (ADR-0071).
                .map_or(ProjectDeployRules::UNGOVERNED, |g| {
                    let effective = g.effective_protection();
                    ProjectDeployRules {
                        gate_deadline: effective.deployment_gate_deadline.millis(),
                        baseline_deadline: g.environment.deployment_deadline.millis(),
                        deployment_concurrency: effective.deployment_concurrency,
                    }
                }),
        };
        self.rules
            .lock()
            .expect("rules cache poisoned")
            .insert(key, rules);
        Ok(rules)
    }

    /// The deadline `project`'s DeploymentGates use in `environment`.
    async fn gate_deadline(
        &self,
        org: &str,
        project: &str,
        environment: &str,
    ) -> Result<Millis, String> {
        Ok(self.rules(org, project, environment).await?.gate_deadline)
    }

    /// The per-DeploymentTarget concurrency hold for deploy Step `step` of
    /// `run` making `requests` — see [`DeploymentGates::deployment_concurrency_hold`].
    /// The first target at its limit, in request order. A Run counted
    /// through only unreached DeploymentGates stops counting once the
    /// Environment's baseline deadline has elapsed by `now` since its
    /// request's effective time (the cutoff `now − baseline`).
    async fn concurrency_hold(
        &self,
        run: &RunId,
        requests: &[RunDeploymentRequest],
        now: Timestamp,
    ) -> Result<Option<DeploymentConcurrencyHold>, String> {
        if requests.is_empty() {
            return Ok(None);
        }
        // The limit is the Run's Project's; without a deploy context there is
        // no Project to read it for. A Run with DeploymentRequests is a Deploy
        // Run and always has one, so this is an anomaly: hold (fail closed).
        let ctx = self
            .db
            .run_deploy_context(run)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                "a run with DeploymentRequests has no deploy context; cannot read its \
                 deployment_concurrency"
                    .to_string()
            })?;
        for request in requests {
            let rules = self
                .rules(&request.org, &ctx.project, &request.environment)
                .await?;
            let Some(limit) = rules.deployment_concurrency else {
                continue;
            };
            let unreached_gate_cutoff = Timestamp(now.0.saturating_sub(rules.baseline_deadline));
            let in_flight = self
                .db
                .deployment_requesting_runs(
                    &request.org,
                    &request.environment,
                    &request.target_source,
                    &request.target_id,
                    run,
                    unreached_gate_cutoff,
                )
                .await
                .map_err(|e| e.to_string())?;
            if !scarab_deployment::admits_deployment_concurrency(Some(limit), in_flight as usize) {
                return Ok(Some(DeploymentConcurrencyHold {
                    target_source: request.target_source.clone(),
                    target_id: request.target_id.clone(),
                    in_flight,
                    limit: limit.get(),
                }));
            }
        }
        Ok(None)
    }

    /// Why `step` of `run` is held by per-DeploymentTarget concurrency right
    /// now, for a view (the run page, b5): `None` when it is not held — it
    /// makes no DeploymentRequest, or every target admits it. Derived, never
    /// read back: nothing is journalled per scheduler pass. Meaningful only
    /// for a `Pending` Step whose needs have all Succeeded (the only Step the
    /// scheduler asks about); the caller checks that. `now` is the control
    /// plane's clock, as the scheduler passes it.
    pub async fn step_concurrency_hold(
        &self,
        run: &RunId,
        step: &StepId,
        now: Timestamp,
    ) -> Result<Option<DeploymentConcurrencyHold>, String> {
        let requests = self
            .db
            .run_deployment_requests(run, step)
            .await
            .map_err(|e| e.to_string())?;
        self.concurrency_hold(run, &requests, now).await
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
        let tick_now = now;
        let (mut cached, mut from_cache) = match self
            .fold(&request.org, &request.environment, tick_now.0)
            .await
        {
            Ok(fold) => fold,
            Err(e) => return unreadable_record(e, window_floor, tick_now, deadline),
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
        let deployment = loop {
            let now = tick_now.0.min(cached.read_now);
            if now < window_floor.0 {
                // The fold predates the window: it cannot speak for it yet.
                return Ok(None);
            }
            if let Some(deployment) = cached.folded.ledger().request_of(&requester, &target) {
                break deployment;
            }
            if from_cache {
                // A fold read earlier in this tick may predate the request's
                // commit (another replica succeeded the deploy Step after
                // it). Clocks cannot settle that — replicas share none — so
                // re-read the record now and decide from that.
                cached = match self
                    .read_fold(&request.org, &request.environment, tick_now.0)
                    .await
                {
                    Ok(fresh) => fresh,
                    Err(e) => return unreadable_record(e, window_floor, tick_now, deadline),
                };
                from_cache = false;
                continue;
            }
            // Missing, decided causally: this gate's wait opened only after
            // this replica read its need `Succeeded`, and the deploy Step's
            // request commits in the SAME transaction as that success — so
            // any read issued after the wait opened sees the request. This
            // fold was read uncached, after the wait opened (by this call),
            // and still lacks it: the request is missing, and waiting cannot
            // bring it.
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
        let now = tick_now.0.min(cached.read_now);
        let ledger = cached.folded.ledger();
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

/// The Deployment record could not be read (the fold errored) — judge Q6 as
/// amended: **no decision** (`Err`, logged by the caller) while the deadline
/// has not elapsed since the window start; at or after it, the gate fails
/// `stalled` with the read error in the reason, so a persistent read error
/// can never make the wait unbounded (criterion 2's deadline clause).
///
/// The window start here is `window_floor`: the window opens at
/// max(request effective_at, Attempt start), and the request's effective_at
/// is the deploy Step Attempt's start, which precedes that Step's success,
/// which is at or before this gate Attempt's start (`window_floor`). So the
/// max is `window_floor` — known without the record.
fn unreadable_record(
    error: String,
    window_floor: Timestamp,
    now: Timestamp,
    deadline: Millis,
) -> Result<Option<DeploymentGateDecision>, String> {
    if now.0 >= window_floor.0.saturating_add(deadline) {
        return Ok(Some(DeploymentGateDecision {
            passed: false,
            reason: format!(
                "stalled: not live within {} (the Deployment record could not be read: {error})",
                deadline_text(deadline)
            ),
            deployment: None,
        }));
    }
    Err(error)
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

    async fn deployment_concurrency_hold(
        &self,
        run: &RunId,
        step: &StepId,
        requests: &[RunDeploymentRequest],
        now: Timestamp,
    ) -> Result<Option<DeploymentConcurrencyHold>, String> {
        let res = self.concurrency_hold(run, requests, now).await;
        if let Err(e) = &res {
            tracing::warn!(
                run = %run.0, step = %step.0, error = %e,
                "deployment concurrency: holding the step this tick (fail closed); asked again next tick"
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
