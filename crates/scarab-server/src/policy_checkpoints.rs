//! The `gate.resolve` consultation (ADR-0075 §2, ADR-0078 §6): one
//! PolicyDecision per approval of a `manual` gate, holding the Environment's
//! release rules as a built-in rule
//! ([`gate_release`](scarab_policy::environment_rules::gate_release)) next
//! to the org's and Project's CEL Policies at `gate.resolve`.
//!
//! A deny refuses the approval before it is recorded. An unmet requirement
//! (approvals missing, the wait timer still running) holds the gate.

use scarab_engine::{Db, DbError, DeployContext, RunId, StepId};
use scarab_policy::evaluate::{evaluate_with_builtins, MatchContext, PolicyDecision, PolicyResult};
use scarab_policy::gate_resolve::{self as gr, GateResolveInput};
use scarab_policy::policy::Checkpoint;
use scarab_policy::run_create::{self as rc, Org, Project, TriggerKind};
use scarab_policy::schema::{check_string_lengths, Capped, Timestamp};
use scarab_policy::store::{DecisionRecord, PolicyStore, StoredPolicy};

/// What a `gate.resolve` consultation of one gate knows besides the
/// approvals and the time.
pub(crate) struct GateResolveFacts {
    org: String,
    project: String,
    gate: Result<gr::GateUnderResolve, String>,
    run: gr::GateRun,
    stored: Vec<StoredPolicy>,
}

fn other(e: impl std::fmt::Display) -> DbError {
    DbError::Other(e.to_string())
}

impl GateResolveFacts {
    /// The facts for gate `step` of `run`, and the effective `gate.resolve`
    /// Policies of the Run's org and Project when `policies` is wired.
    /// `None` for a Run with no Project, which no Policy can match.
    pub(crate) async fn load(
        db: &dyn Db,
        policies: Option<&dyn PolicyStore>,
        run: &RunId,
        step: &StepId,
        deploy: Option<&DeployContext>,
        approval_gate: bool,
    ) -> Result<Option<GateResolveFacts>, DbError> {
        let Some((org, project)) = db.run_tenant(run).await? else {
            return Ok(None);
        };
        let stored = match policies {
            Some(p) => p
                .effective(&org, Some(&project), Checkpoint::GateResolve)
                .await
                .map_err(|e| other(format!("policy store: {e}")))?,
            None => Vec::new(),
        };
        let ir: Option<scarab_pipeline::PipelineIr> = db
            .run_ir(run)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| other(format!("the Run's stored pipeline cannot be read: {e}")))?;
        let gate = match ir
            .as_ref()
            .and_then(|ir| ir.steps.iter().find(|s| s.id == step.0).map(|s| (ir, s)))
        {
            None => Err(format!(
                "gate `{}` is not in the Run's stored pipeline",
                step.0
            )),
            Some((ir, spec)) => gate_of(ir, spec, approval_gate).map_err(|e| e.to_string()),
        };
        Ok(Some(GateResolveFacts {
            org,
            project,
            gate,
            run: gr::GateRun {
                id: run.0.clone(),
                r#ref: deploy.map(|c| c.git_ref.clone()),
                environment: deploy.map(|c| c.environment.clone()),
            },
            stored,
        }))
    }

    /// The decision for `actor`'s approval, with `approvals` the current
    /// wait's approvals (the actor's included) and `builtin` the Environment's
    /// release rule's result when it applies. CPU work, run off the async
    /// workers.
    pub(crate) async fn decide(
        &self,
        actor: &str,
        approvals: &[String],
        now_ms: i64,
        builtin: Option<PolicyResult>,
    ) -> Result<PolicyDecision, DbError> {
        let input = self.input(actor, approvals, now_ms);
        let ctx = MatchContext {
            project: self.project.clone(),
            environment: self.run.environment.clone(),
            // `match.trigger_kinds` is refused outside `run.create`, so this
            // is never read.
            trigger_kind: TriggerKind::Manual,
        };
        let stored = self.stored.clone();
        tokio::task::spawn_blocking(move || {
            evaluate_with_builtins(
                Checkpoint::GateResolve,
                gr::SCHEMA_VERSION,
                input.as_ref().map_err(String::as_str),
                None,
                &ctx,
                &stored,
                builtin.into_iter().collect(),
            )
        })
        .await
        .map_err(other)
    }

    fn input(
        &self,
        actor: &str,
        approvals: &[String],
        now_ms: i64,
    ) -> Result<serde_json::Value, String> {
        let gate = self.gate.clone()?;
        let input = GateResolveInput {
            org: Org {
                name: self.org.clone(),
            },
            project: Project {
                name: self.project.clone(),
            },
            gate,
            verdict: gr::Verdict::Release,
            actor: gr::Actor {
                principal: actor.to_string(),
            },
            approvals: Capped::new(approvals.to_vec()).ok_or_else(|| {
                format!(
                    "the gate has {} approvals, more than the {} a gate.resolve Policy can \
                     check",
                    approvals.len(),
                    gr::MAX_APPROVALS
                )
            })?,
            run: self.run.clone(),
            time: Timestamp::from_unix_millis(now_ms)
                .ok_or_else(|| format!("the time {now_ms} is out of range"))?,
        };
        let value = serde_json::to_value(input).map_err(|e| e.to_string())?;
        check_string_lengths(&gr::schema(), &value).map_err(|e| {
            format!(
                "`{}` is {} bytes, more than the {} a gate.resolve Policy can check",
                e.path, e.len, e.max
            )
        })?;
        Ok(value)
    }

    /// Records `decision` when a store is wired: always when a Policy was
    /// evaluated, and when only the built-in rule was, too.
    pub(crate) async fn record(
        &self,
        policies: Option<&dyn PolicyStore>,
        decision: PolicyDecision,
        now_ms: i64,
    ) -> Result<Option<String>, DbError> {
        let Some(policies) = policies else {
            return Ok(None);
        };
        let record = DecisionRecord {
            id: uuid::Uuid::now_v7().to_string(),
            org: self.org.clone(),
            project: Some(self.project.clone()),
            run_id: Some(self.run.id.clone()),
            step_id: self.gate.as_ref().ok().map(|g| g.id.clone()),
            attempt: None,
            decision,
            at: now_ms,
        };
        policies
            .record_decision(&record)
            .await
            .map(Some)
            .map_err(|e| other(format!("policy store: {e}")))
    }

    /// Whether a Policy is stored at `gate.resolve` for this Run.
    pub(crate) fn has_policies(&self) -> bool {
        !self.stored.is_empty()
    }
}

/// The gate's part of the input. Its dependents are the Steps that need it
/// directly.
fn gate_of(
    ir: &scarab_pipeline::PipelineIr,
    spec: &scarab_pipeline::StepSpec,
    approval_gate: bool,
) -> Result<gr::GateUnderResolve, rc::CapExceeded> {
    let what = || format!("gate `{}`", spec.id);
    let dependents: Vec<String> = ir
        .steps
        .iter()
        .filter(|s| s.needs.0.contains(&spec.id))
        .map(|s| s.id.clone())
        .collect();
    Ok(gr::GateUnderResolve {
        id: spec.id.clone(),
        kind: rc::gate_kind(spec)?,
        origin: rc::origin_of(&spec.origin, &what)?,
        environment_approval_gate: approval_gate,
        dependents: rc::capped(dependents, &what, "dependent Steps")?,
    })
}
