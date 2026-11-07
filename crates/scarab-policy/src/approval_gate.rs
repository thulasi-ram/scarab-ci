//! The **Environment approval gate** (ADR-0078 §1-2): which `manual` gates
//! of a Run are Environment approval gates, which Steps that use the
//! Environment's authority have no `manual` gate upstream, and the one gate
//! the Environment adds in front of them.
//!
//! One definition of "uses the Environment's authority" holds for all of
//! it: [`uses_environment_authority`].

use std::collections::{BTreeSet, HashMap};

use scarab_pipeline::{OriginEntry, OriginKind, PipelineIr, StepSpec};

use crate::run_create::uses_environment_authority;

/// The id the Environment's added gate takes, or, when a Step of the Run
/// already has it, this id with `-2`, `-3`, ... appended.
pub const ADDED_GATE_ID: &str = "environment-approval";

fn is_manual_gate(s: &StepSpec) -> bool {
    s.gate.as_deref() == Some("manual")
}

/// Whether a `gate: manual` is upstream of Step `step` of `ir`, through
/// `needs`, transitively.
pub fn manual_gate_upstream(ir: &PipelineIr, step: &str) -> bool {
    let by_id: HashMap<&str, &StepSpec> = ir.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = by_id
        .get(step)
        .map(|s| s.needs.0.iter().map(String::as_str).collect())
        .unwrap_or_default();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(s) = by_id.get(id) else { continue };
        if is_manual_gate(s) {
            return true;
        }
        stack.extend(s.needs.0.iter().map(String::as_str));
    }
    false
}

/// The `manual` gates upstream of Step `step` of `ir` (through `needs`,
/// transitively) that count as Environment approval gates: marked one, or of
/// a Run created before the mark existed (unmarked), in Pipeline order.
pub fn upstream_approval_gates<'a>(ir: &'a PipelineIr, step: &str) -> Vec<&'a StepSpec> {
    let by_id: HashMap<&str, &StepSpec> = ir.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = by_id
        .get(step)
        .map(|s| s.needs.0.iter().map(String::as_str).collect())
        .unwrap_or_default();
    while let Some(id) = stack.pop() {
        if seen.insert(id) {
            if let Some(s) = by_id.get(id) {
                stack.extend(s.needs.0.iter().map(String::as_str));
            }
        }
    }
    ir.steps
        .iter()
        .filter(|s| seen.contains(s.id.as_str()))
        .filter(|s| s.counts_as_environment_approval_gate())
        .collect()
}

/// The ids of every Step of `ir` downstream of `step` (through `needs`,
/// transitively), `step` excluded.
fn downstream<'a>(ir: &'a PipelineIr, step: &str) -> BTreeSet<&'a str> {
    let mut needed_by: HashMap<&str, Vec<&str>> = HashMap::new();
    for s in &ir.steps {
        for n in &s.needs.0 {
            needed_by.entry(n.as_str()).or_default().push(s.id.as_str());
        }
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = needed_by.get(step).cloned().unwrap_or_default();
    while let Some(id) = stack.pop() {
        if seen.insert(id) {
            stack.extend(needed_by.get(id).into_iter().flatten().copied());
        }
    }
    seen
}

/// The `manual` gates of `ir` that are Environment approval gates: a Step
/// after the gate (through `needs`, transitively) uses the Environment's
/// authority. In Pipeline order.
pub fn environment_approval_gates(ir: &PipelineIr) -> Vec<String> {
    let by_id: HashMap<&str, &StepSpec> = ir.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    ir.steps
        .iter()
        .filter(|g| is_manual_gate(g))
        .filter(|g| {
            downstream(ir, &g.id).into_iter().any(|d| {
                by_id
                    .get(d)
                    .is_some_and(|s| uses_environment_authority(ir, s))
            })
        })
        .map(|g| g.id.clone())
        .collect()
}

/// Mark each `manual` gate of `ir` with whether it is an Environment
/// approval gate ([`StepSpec::environment_approval_gate`]), for a Run that
/// an Environment governs. Run creation calls it once; the mark is stored
/// with the Run and never decided again.
pub fn mark_environment_approval_gates(ir: &mut PipelineIr) {
    let approval: BTreeSet<String> = environment_approval_gates(ir).into_iter().collect();
    for s in ir.steps.iter_mut().filter(|s| is_manual_gate(s)) {
        s.environment_approval_gate = Some(approval.contains(&s.id));
    }
}

/// The Steps of `ir` that use the Environment's authority with no `manual`
/// gate upstream, in Pipeline order. A Step in `excluded` (a `when:` that
/// is false, so the Step starts Skipped) is left out.
pub fn steps_missing_approval_gate(ir: &PipelineIr, excluded: &[String]) -> Vec<String> {
    ir.steps
        .iter()
        .filter(|s| uses_environment_authority(ir, s))
        .filter(|s| !excluded.contains(&s.id))
        .filter(|s| !manual_gate_upstream(ir, &s.id))
        .map(|s| s.id.clone())
        .collect()
}

/// The id the added gate takes in `ir` ([`ADDED_GATE_ID`]).
pub fn added_gate_id(ir: &PipelineIr) -> String {
    let taken = |id: &str| ir.steps.iter().any(|s| s.id == id);
    if !taken(ADDED_GATE_ID) {
        return ADDED_GATE_ID.to_string();
    }
    (2..)
        .map(|n| format!("{ADDED_GATE_ID}-{n}"))
        .find(|id| !taken(id))
        .expect("an unused suffix exists")
}

/// What [`add_environment_approval_gate`] added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedGate {
    /// The gate's id.
    pub id: String,
    /// The Steps whose `needs` now name it.
    pub fronts: Vec<String>,
}

/// Add the Environment's approval gate to `ir` (ADR-0078 §2): one `manual`
/// gate in front of the earliest Steps that use the Environment's authority
/// with no `manual` gate upstream ([`steps_missing_approval_gate`]) — those
/// with no other such Step upstream. Its `needs` are theirs combined, each
/// of them gets it added to its `needs`, and its `origin` names the
/// Environment. Every later such Step is downstream of one of them, so
/// behind the gate, and none of the gate's `needs` is downstream of one of
/// them, so the gate forms no cycle. `None` when no Step needs it.
///
/// With `deploy: true` on the Pipeline, every Step uses the authority, so
/// the earliest ones are the roots and the gate has no `needs`.
pub fn add_environment_approval_gate(
    ir: &mut PipelineIr,
    environment: &str,
    expires_after: Option<u32>,
    excluded: &[String],
) -> Option<AddedGate> {
    let missing = steps_missing_approval_gate(ir, excluded);
    if missing.is_empty() {
        return None;
    }
    let missing_set: BTreeSet<&str> = missing.iter().map(String::as_str).collect();
    let behind_another: BTreeSet<&str> = missing
        .iter()
        .flat_map(|m| downstream(ir, m))
        .filter(|d| missing_set.contains(d))
        .collect();
    let fronts: Vec<String> = missing
        .iter()
        .filter(|m| !behind_another.contains(m.as_str()))
        .cloned()
        .collect();
    let mut needs: Vec<String> = Vec::new();
    for s in ir.steps.iter().filter(|s| fronts.contains(&s.id)) {
        for n in &s.needs.0 {
            if !needs.contains(n) {
                needs.push(n.clone());
            }
        }
    }
    let id = added_gate_id(ir);
    let at = ir
        .steps
        .iter()
        .position(|s| fronts.contains(&s.id))
        .unwrap_or(ir.steps.len());
    for s in ir.steps.iter_mut().filter(|s| fronts.contains(&s.id)) {
        s.needs.0.push(id.clone());
    }
    // Every other field at its default, as a gate written `{ id, gate:
    // manual }` has it.
    let mut gate: StepSpec =
        serde_json::from_value(serde_json::json!({ "id": id, "gate": "manual" }))
            .expect("a bare manual gate decodes");
    gate.gate_expires_after = expires_after;
    gate.needs = scarab_pipeline::Needs(needs);
    gate.origin = vec![OriginEntry {
        kind: OriginKind::Environment,
        name: environment.to_string(),
        source: None,
        content_hash: String::new(),
    }];
    gate.environment_approval_gate = Some(true);
    ir.steps.insert(at, gate);
    Some(AddedGate { id, fronts })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ir(yaml: &str) -> PipelineIr {
        scarab_pipeline::compile_yaml(yaml).expect("compiles")
    }

    fn step<'a>(ir: &'a PipelineIr, id: &str) -> &'a StepSpec {
        ir.steps.iter().find(|s| s.id == id).expect(id)
    }

    fn needs(ir: &PipelineIr, id: &str) -> Vec<String> {
        step(ir, id).needs.0.clone()
    }

    /// Only a `manual` gate with an authority Step after it is an
    /// Environment approval gate; a "QA signs off" gate in front of a smoke
    /// test that holds nothing of the Environment's is a plain gate.
    #[test]
    fn a_manual_gate_is_an_approval_gate_only_with_an_authority_step_after_it() {
        let mut ir = ir(r#"
environment: prod
steps:
  - { id: build, image: busybox }
  - { id: qa, gate: manual, needs: [build] }
  - { id: smoke, image: busybox, needs: [qa] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: wait, gate: timer, gate_after: 60, needs: [build] }
  - { id: migrate, image: busybox, needs: [approve] }
  - { id: ship, image: busybox, secrets: [TOKEN], needs: [migrate, wait] }
"#);
        assert_eq!(environment_approval_gates(&ir), vec!["approve".to_string()]);
        mark_environment_approval_gates(&mut ir);
        assert_eq!(step(&ir, "approve").environment_approval_gate, Some(true));
        assert_eq!(step(&ir, "qa").environment_approval_gate, Some(false));
        assert_eq!(
            step(&ir, "wait").environment_approval_gate,
            None,
            "manual only"
        );
        assert_eq!(step(&ir, "build").environment_approval_gate, None);
    }

    /// The added gate goes in front of the earliest authority Steps with no
    /// `manual` gate upstream, with their `needs` combined; a later one is
    /// already behind it.
    #[test]
    fn the_added_gate_fronts_the_earliest_authority_steps() {
        let mut ir = ir(r#"
environment: prod
steps:
  - { id: build, image: busybox }
  - { id: test, image: busybox }
  - { id: eu, image: busybox, secrets: [TOKEN], needs: [build] }
  - { id: us, image: busybox, deploy: true, needs: [build, test] }
  - { id: verify, image: busybox, secrets: [TOKEN], needs: [eu, us] }
"#);
        assert_eq!(
            steps_missing_approval_gate(&ir, &[]),
            vec!["eu".to_string(), "us".to_string(), "verify".to_string()]
        );
        let added = add_environment_approval_gate(&mut ir, "prod", Some(600), &[]).unwrap();
        assert_eq!(added.id, ADDED_GATE_ID);
        assert_eq!(added.fronts, vec!["eu".to_string(), "us".to_string()]);
        let gate = step(&ir, ADDED_GATE_ID);
        assert_eq!(gate.gate.as_deref(), Some("manual"));
        assert_eq!(gate.gate_expires_after, Some(600));
        assert_eq!(gate.needs.0, vec!["build".to_string(), "test".to_string()]);
        assert_eq!(gate.origin.len(), 1);
        assert_eq!(gate.origin[0].kind, OriginKind::Environment);
        assert_eq!(gate.origin[0].name, "prod");
        assert_eq!(gate.environment_approval_gate, Some(true));
        assert_eq!(needs(&ir, "eu"), vec!["build", ADDED_GATE_ID]);
        assert_eq!(needs(&ir, "us"), vec!["build", "test", ADDED_GATE_ID]);
        assert_eq!(needs(&ir, "verify"), vec!["eu", "us"], "already behind it");
        assert!(steps_missing_approval_gate(&ir, &[]).is_empty());
        // The gate sits before the first Step it fronts.
        let at = |id: &str| ir.steps.iter().position(|s| s.id == id).unwrap();
        assert!(at(ADDED_GATE_ID) < at("eu"));
        assert!(scarab_pipeline::validate(&ir).is_ok(), "a valid DAG");
    }

    /// With `deploy: true` on the Pipeline every Step uses the authority, so
    /// the gate comes first, with no `needs`.
    #[test]
    fn a_deploy_pipeline_gets_the_gate_before_everything() {
        let mut ir = ir(r#"
environment: prod
deploy: true
steps:
  - { id: build, image: busybox }
  - { id: ship, image: busybox, needs: [build] }
"#);
        let added = add_environment_approval_gate(&mut ir, "prod", None, &[]).unwrap();
        assert_eq!(added.fronts, vec!["build".to_string()]);
        assert!(step(&ir, ADDED_GATE_ID).needs.0.is_empty());
        assert_eq!(ir.steps[0].id, ADDED_GATE_ID);
    }

    /// No gate when every authority Step already has a `manual` gate
    /// upstream, or is excluded by its `when:`; a colliding id gets a
    /// suffix.
    #[test]
    fn no_gate_when_none_is_missing_and_the_id_never_collides() {
        let mut gated = ir(r#"
environment: prod
steps:
  - { id: approve, gate: manual }
  - { id: ship, image: busybox, secrets: [TOKEN], needs: [approve] }
"#);
        assert_eq!(
            add_environment_approval_gate(&mut gated, "prod", None, &[]),
            None
        );
        let mut excluded = ir(r#"
environment: prod
steps:
  - { id: ship, image: busybox, secrets: [TOKEN] }
"#);
        assert_eq!(
            add_environment_approval_gate(&mut excluded, "prod", None, &["ship".to_string()]),
            None
        );
        let mut taken = ir(r#"
environment: prod
steps:
  - { id: environment-approval, image: busybox }
  - { id: environment-approval-2, image: busybox }
  - { id: ship, image: busybox, secrets: [TOKEN], needs: [environment-approval] }
"#);
        let added = add_environment_approval_gate(&mut taken, "prod", None, &[]).unwrap();
        assert_eq!(added.id, "environment-approval-3");
    }

    #[test]
    fn manual_gate_upstream_is_transitive_and_manual_only() {
        let ir = ir(r#"
steps:
  - { id: approve, gate: manual }
  - { id: wait, gate: timer, gate_after: 5 }
  - { id: a, image: busybox, needs: [approve] }
  - { id: b, image: busybox, needs: [a] }
  - { id: c, image: busybox, needs: [wait] }
"#);
        assert!(manual_gate_upstream(&ir, "b"));
        assert!(!manual_gate_upstream(&ir, "c"));
        assert!(!manual_gate_upstream(&ir, "approve"));
    }
}
