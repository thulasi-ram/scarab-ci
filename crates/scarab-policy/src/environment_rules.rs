//! The Environment's approval-gate rules as built-in rules of the engine
//! (ADR-0078 §6, ADR-0075 §12): the first typed settings the engine
//! evaluates. The settings stay typed and are edited where they were
//! (`EffectiveProtection`, the Environment's baseline and each Project
//! overlay); each checkpoint evaluates them here, and the
//! [`PolicyResult`] each returns goes into that checkpoint's
//! [`PolicyDecision`](crate::evaluate::PolicyDecision) next to the CEL
//! Policies' results ([`evaluate_with_builtins`](crate::evaluate::evaluate_with_builtins)).
//!
//! - `run.create` ([`gate_requirement`]): a Step that uses the Environment's
//!   authority with no `manual` gate upstream gets the Environment's gate,
//!   or, in missing-gate mode `refuse`, the Run is refused.
//! - `gate.resolve` ([`gate_release`]): an Environment approval gate
//!   releases when its approvals meet the Environment's approvers and
//!   required count, the Run's ref is allowed, and the wait timer has run
//!   out on the gate's wait clock.
//! - `step.admit` ([`launch_recheck`]): at every launch of a Step that uses
//!   the Environment's authority, an Environment approval gate upstream has
//!   released, the ref is still allowed, the approver set is the one the
//!   release recorded, and the release is within the maximum approval age.
//!   A Step with no released approval gate upstream gets the `env=none`
//!   OIDC subject.
//!
//! A built-in rule's result names it by a `builtin:` id, as the plugin
//! default's does ([`PLUGINS_DEFAULT_ID`](crate::evaluate::PLUGINS_DEFAULT_ID)).
//! Being fixed in code, it is not part of the PolicySetDigest.

use scarab_pipeline::PipelineIr;
use scarab_project::{EffectiveProtection, MissingGateMode};

use crate::approval_gate as ag;
use crate::evaluate::{PolicyOutcome, PolicyResult};
use crate::ir::Level;
use crate::policy::Mode;

/// The `policy_id` of the `run.create` rule: the gate requirement.
pub const GATE_REQUIREMENT_ID: &str = "builtin:environment_approval_gate";
/// The `policy_id` of the `gate.resolve` rule: the release rules.
pub const GATE_RELEASE_ID: &str = "builtin:environment_approval_release";
/// The `policy_id` of the `step.admit` rule: the launch-time re-check.
pub const LAUNCH_RECHECK_ID: &str = "builtin:environment_approval_recheck";

/// A built-in rule's result: enforced, at the org level (an Environment's
/// baseline is the org's; a Project overlay only tightens it).
fn builtin(id: &str, name: &str, outcome: PolicyOutcome) -> PolicyResult {
    PolicyResult {
        policy_id: id.to_string(),
        version: 0,
        name: name.to_string(),
        level: Level::Org,
        mode: Mode::Enforce,
        outcome,
    }
}

/// What the gate requirement decided for a Run.
#[derive(Clone, Debug, PartialEq)]
pub struct GateRequirement {
    /// The Run as it is stored: with the Environment's gate when one was
    /// added, every `manual` gate marked an Environment approval gate or not.
    pub ir: PipelineIr,
    /// The rule's result; `None` when it does not apply (the Environment has
    /// no approvers).
    pub result: Option<PolicyResult>,
    /// In missing-gate mode `refuse`: why the Run is refused, naming the YAML
    /// to add.
    pub refusal: Option<String>,
}

/// The gate requirement at `run.create` (ADR-0078 §1-3), for a Run governed
/// by `environment` with `protection` as its effective rules. Where the
/// Environment has approvers and a Step uses its authority with no `manual`
/// gate upstream, the Environment adds one gate in front of the earliest
/// such Steps, or, in missing-gate mode `refuse`, the Run is refused. Then
/// every `manual` gate is marked with whether it is an Environment approval
/// gate. Steps in `excluded` (a `when:` that is false) are left out.
pub fn gate_requirement(
    ir: &PipelineIr,
    environment: &str,
    protection: Option<&EffectiveProtection>,
    excluded: &[String],
) -> GateRequirement {
    let mut ir = ir.clone();
    let mut result = None;
    if let Some(p) = protection.filter(|p| !p.approvers.is_empty()) {
        let settings = &p.approval_gate;
        let mut with_gate = ir.clone();
        let outcome = match ag::add_environment_approval_gate(
            &mut with_gate,
            environment,
            settings.added_gate_expiry_s,
            excluded,
        ) {
            None => PolicyOutcome::Passed,
            Some(added) if settings.missing_gate == MissingGateMode::Refuse => {
                let gate = with_gate.steps.iter().find(|s| s.id == added.id);
                let needs = gate.map(|g| g.needs.0.join(", ")).unwrap_or_default();
                let quote = |ids: &[String]| {
                    ids.iter()
                        .map(|i| format!("`{i}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let missing = ag::steps_missing_approval_gate(&ir, excluded);
                let refusal = format!(
                    "environment `{environment}` has approvers, and these Steps use its \
                     authority with no `gate: manual` upstream: {}. It refuses such a Run \
                     rather than add a gate: add `- {{ id: approve, gate: manual, needs: \
                     [{needs}] }}` and put `approve` in the `needs` of {}",
                    quote(&missing),
                    quote(&added.fronts),
                );
                ag::mark_environment_approval_gates(&mut ir);
                return GateRequirement {
                    ir,
                    result: Some(builtin(
                        GATE_REQUIREMENT_ID,
                        "environment-approval-gate",
                        PolicyOutcome::Denied {
                            messages: vec![refusal.clone()],
                        },
                    )),
                    refusal: Some(refusal),
                };
            }
            Some(added) => {
                ir = with_gate;
                PolicyOutcome::GateAdded {
                    gate: added.id,
                    fronts: added.fronts,
                }
            }
        };
        result = Some(builtin(
            GATE_REQUIREMENT_ID,
            "environment-approval-gate",
            outcome,
        ));
    }
    ag::mark_environment_approval_gates(&mut ir);
    GateRequirement {
        ir,
        result,
        refusal: None,
    }
}

/// What the release rules of an Environment approval gate read.
#[derive(Clone, Copy, Debug)]
pub struct GateReleaseFacts<'a> {
    /// The Run's symbolic ref.
    pub git_ref: &'a str,
    /// The Environment's effective rules for the Run's Project, now.
    pub protection: &'a EffectiveProtection,
    /// The Principals who have approved in the gate's current wait.
    pub approvals: &'a [String],
    /// When the gate's current wait began (unix ms).
    pub wait_began_ms: i64,
    pub now_ms: i64,
}

/// The approvals of an Environment approval gate met the rules: the approver
/// set they met, which every launch behind the gate compares against, and
/// when the wait timer runs out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalMet {
    pub approvers: Vec<String>,
    pub required: Option<u32>,
    pub release_at: i64,
}

/// What the release rules decided for one approval.
#[derive(Clone, Debug, PartialEq)]
pub struct GateRelease {
    /// [`PolicyOutcome::Passed`] when the gate may release now,
    /// [`PolicyOutcome::Requires`] with what it still lacks otherwise.
    pub result: PolicyResult,
    /// `Some` once the approvals meet the rules, even while the wait timer
    /// holds the gate.
    pub met: Option<ApprovalMet>,
}

/// The release rules of an Environment approval gate at `gate.resolve`
/// (ADR-0078 §1, §4): the ref is allowed and the approvals meet the
/// Environment's approvers and required count (`EffectiveProtection::admits`),
/// and the wait timer, counted from the start of the gate's current wait,
/// has run out. Approvals given before the timer runs out count.
pub fn gate_release(f: GateReleaseFacts<'_>) -> GateRelease {
    let release = |outcome| builtin(GATE_RELEASE_ID, "environment-approval-release", outcome);
    if let Err(violations) = f.protection.admits(f.git_ref, f.approvals) {
        return GateRelease {
            result: release(PolicyOutcome::Requires {
                messages: violations,
            }),
            met: None,
        };
    }
    let timer_s = f.protection.effective_wait_timer_s();
    let release_at = f
        .wait_began_ms
        .saturating_add(i64::from(timer_s).saturating_mul(1000));
    let (approvers, required) = f.protection.approver_set();
    let outcome = if f.now_ms < release_at {
        PolicyOutcome::Requires {
            messages: vec![format!(
                "the approvals are in; the wait timer of {timer_s} s runs for {} s more",
                (release_at - f.now_ms + 999) / 1000
            )],
        }
    } else {
        PolicyOutcome::Passed
    };
    GateRelease {
        result: release(outcome),
        met: Some(ApprovalMet {
            approvers,
            required,
            release_at,
        }),
    }
}

/// One Environment approval gate upstream of a Step being launched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamGateFacts {
    pub gate: String,
    /// When the gate released (unix ms); `None` while it has not.
    pub released_at: Option<i64>,
    /// The approver set its approvals met and the required count, when the
    /// release recorded them (a gate released before that record existed
    /// has none).
    pub met: Option<(Vec<String>, Option<u32>)>,
}

/// What the launch-time re-check reads.
#[derive(Clone, Copy, Debug)]
pub struct LaunchFacts<'a> {
    pub environment: &'a str,
    /// The Run's symbolic ref.
    pub git_ref: &'a str,
    /// The Environment's effective rules for the Run's Project, now.
    pub protection: &'a EffectiveProtection,
    pub step: &'a str,
    /// Whether the Step uses the Environment's authority; `None` when the
    /// Run has no stored pipeline or the Step is not in it.
    pub uses_authority: Option<bool>,
    /// The Environment approval gates upstream of the Step, in Pipeline
    /// order.
    pub approval_gates: &'a [UpstreamGateFacts],
    pub now_ms: i64,
}

/// What the launch-time re-check decided.
#[derive(Clone, Debug, PartialEq)]
pub struct LaunchCheck {
    /// The rule's result; `None` when the Environment has no approvers.
    pub result: Option<PolicyResult>,
    /// Why the Step may not use the Environment's authority now.
    pub refusal: Option<String>,
    /// No Environment approval gate upstream has released, in an
    /// Environment with approvers: the Step's OIDC subject is `env=none`
    /// (ADR-0075 §3).
    pub before_approval: bool,
}

/// The launch-time re-check at `step.admit` (ADR-0075 §3, ADR-0078 §5), at
/// every launch, reruns and retries included. In an Environment with
/// approvers, a Step that uses its authority is refused unless an
/// Environment approval gate is upstream of it and released, the Run's ref
/// is still allowed, each upstream release recorded the approver set the
/// Environment has now, and none is older than the maximum approval age.
/// External, timer and plugin gates are never approval gates, so a Step
/// behind only those is refused whatever released them.
pub fn launch_recheck(f: LaunchFacts<'_>) -> LaunchCheck {
    let p = f.protection;
    if p.approvers.is_empty() {
        return LaunchCheck {
            result: None,
            refusal: None,
            before_approval: false,
        };
    }
    let before_approval = !f.approval_gates.iter().any(|g| g.released_at.is_some());
    let refusal = (f.uses_authority == Some(true))
        .then(|| refusal(f))
        .flatten();
    let outcome = match &refusal {
        None => PolicyOutcome::Passed,
        Some(r) => PolicyOutcome::Denied {
            messages: vec![r.clone()],
        },
    };
    LaunchCheck {
        result: Some(builtin(
            LAUNCH_RECHECK_ID,
            "environment-approval-recheck",
            outcome,
        )),
        refusal,
        before_approval,
    }
}

fn refusal(f: LaunchFacts<'_>) -> Option<String> {
    let (env, id, p) = (f.environment, f.step, f.protection);
    if f.approval_gates.is_empty() {
        return Some(format!(
            "step `{id}` uses environment `{env}`'s authority, and no Environment approval gate \
             is upstream of it: the Run was created before `{env}` had approvers, or no `gate: \
             manual` guards the step; start a new Run so `{env}` can add its gate"
        ));
    }
    if !p.ref_allowed(f.git_ref) {
        return Some(format!(
            "ref `{}` is no longer allowed to deploy to environment `{env}`",
            f.git_ref
        ));
    }
    let max_age_ms = i64::from(p.approval_gate.max_approval_age_s) * 1000;
    for gate in f.approval_gates {
        let Some(released) = gate.released_at else {
            return Some(format!(
                "Environment approval gate `{}` upstream of step `{id}` has not released",
                gate.gate
            ));
        };
        let age_ms = f.now_ms.saturating_sub(released);
        if age_ms > max_age_ms {
            return Some(format!(
                "Environment approval gate `{}` released {} s ago, longer than environment \
                 `{env}`'s maximum approval age of {} s; rerun the gate to ask the approvers \
                 again",
                gate.gate,
                age_ms / 1000,
                p.approval_gate.max_approval_age_s
            ));
        }
        if let Some(met) = &gate.met {
            if *met != p.approver_set() {
                return Some(format!(
                    "environment `{env}`'s approvers changed since Environment approval gate \
                     `{}` released; rerun the gate to ask the approvers again",
                    gate.gate
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_project::{ApprovalGateRules, EnvironmentProtectionRules};

    fn protection(approvers: &[&str], refuse: bool) -> EffectiveProtection {
        let rules = EnvironmentProtectionRules {
            approvers: approvers.iter().map(|a| a.to_string()).collect(),
            approval_gate: ApprovalGateRules {
                missing_gate: if refuse {
                    MissingGateMode::Refuse
                } else {
                    MissingGateMode::Add
                },
                ..Default::default()
            },
            ..Default::default()
        };
        scarab_project::effective_protection(&rules, None)
    }

    #[test]
    fn the_release_rules_hold_for_approvals_then_for_the_timer() {
        let mut p = protection(&["amy", "bo"], false);
        p.wait_timer_s = 60;
        let facts = |approvals: &[String], now_ms| {
            gate_release(GateReleaseFacts {
                git_ref: "refs/heads/main",
                protection: &p,
                approvals,
                wait_began_ms: 1_000,
                now_ms,
            })
        };
        let one = facts(&["amy".to_string()], 2_000);
        assert_eq!(one.result.policy_id, GATE_RELEASE_ID);
        assert!(
            matches!(&one.result.outcome, PolicyOutcome::Requires { messages }
            if messages[0].contains("`bo`"))
        );
        assert_eq!(one.met, None);

        let both = ["amy".to_string(), "bo".to_string()];
        let early = facts(&both, 2_000);
        assert!(matches!(
            early.result.outcome,
            PolicyOutcome::Requires { .. }
        ));
        assert_eq!(early.met.as_ref().unwrap().release_at, 61_000);
        let late = facts(&both, 61_000);
        assert_eq!(late.result.outcome, PolicyOutcome::Passed);
    }

    const UNGATED: &str = r#"
environment: prod
steps:
  - { id: build, image: busybox }
  - { id: ship, image: busybox, deploy: true, needs: [build] }
"#;

    #[test]
    fn the_gate_requirement_names_itself_in_its_result() {
        let ir = scarab_pipeline::compile_yaml(UNGATED).unwrap();
        let added = gate_requirement(&ir, "prod", Some(&protection(&["amy"], false)), &[]);
        let r = added.result.expect("applies");
        assert_eq!(r.policy_id, GATE_REQUIREMENT_ID);
        assert_eq!(
            r.outcome,
            PolicyOutcome::GateAdded {
                gate: ag::ADDED_GATE_ID.into(),
                fronts: vec!["ship".into()]
            }
        );
        assert!(added.refusal.is_none());
        assert!(added.ir.steps.iter().any(|s| s.id == ag::ADDED_GATE_ID));

        let refused = gate_requirement(&ir, "prod", Some(&protection(&["amy"], true)), &[]);
        assert!(matches!(
            refused.result.unwrap().outcome,
            PolicyOutcome::Denied { .. }
        ));
        assert!(refused.refusal.unwrap().contains("refuses such a Run"));

        let none = gate_requirement(&ir, "prod", Some(&protection(&[], false)), &[]);
        assert!(
            none.result.is_none(),
            "no approvers: the rule does not apply"
        );
    }
}
