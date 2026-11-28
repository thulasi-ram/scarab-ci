//! The **Fork Run approval gate** (ADR-0086 §9): the one `manual` gate the
//! org's Fork Run approval setting (`off`, `agent_started`, `all`) puts in
//! front of every root Step of a Run of fork code it holds, so nothing in
//! the Run starts, its shared services included,
//! until someone with write on the repository releases it.
//!
//! It is never an Environment approval gate
//! ([`StepSpec::counts_as_environment_approval_gate`](scarab_pipeline::StepSpec::counts_as_environment_approval_gate)):
//! it does not stand in for the gate an Environment adds, and that gate does
//! not stand in for it.

use scarab_pipeline::{OriginEntry, OriginKind, PipelineIr, StepSpec};

/// The id the gate takes, or, when a Step of the Run already has it, this
/// id with `-2`, `-3`, ... appended.
pub const FORK_RUN_APPROVAL_GATE_ID: &str = "fork-run-approval";

/// How long the gate waits before it fails the Run (class `gate`): 72 hours.
pub const FORK_RUN_APPROVAL_GATE_EXPIRES_AFTER_S: u32 = 72 * 60 * 60;

/// The id the gate takes in `ir` ([`FORK_RUN_APPROVAL_GATE_ID`]).
fn gate_id(ir: &PipelineIr) -> String {
    let taken = |id: &str| ir.steps.iter().any(|s| s.id == id);
    if !taken(FORK_RUN_APPROVAL_GATE_ID) {
        return FORK_RUN_APPROVAL_GATE_ID.to_string();
    }
    (2..)
        .map(|n| format!("{FORK_RUN_APPROVAL_GATE_ID}-{n}"))
        .find(|id| !taken(id))
        .expect("an unused suffix exists")
}

/// Add the Fork Run approval gate to `ir`, named in its `origin` by
/// `setting` (the setting that applied): one `manual` gate with no `needs`,
/// first in the Pipeline, that every root Step (a Step with no `needs`) now
/// needs, so every Step of the Run is behind it. A root Step in `excluded`
/// (a `when:` that is false, so it starts Skipped) is left alone. Run
/// creation calls it last, after the Environment's gate requirement, so an
/// Environment approval gate the Environment added as a root is behind it
/// too. Returns the gate's id; `None` when no root Step would start, which
/// leaves `ir` unchanged.
///
/// The gate: `gate: manual`, `gate_expires_after` 72 hours,
/// `environment_approval_gate: false`, and one `origin` link of kind
/// `fork_run_approval_gate` with an empty `content_hash`. Every other field
/// is at its default, as a gate written `{ id, gate: manual }` has it.
pub fn add_fork_run_approval_gate(
    ir: &mut PipelineIr,
    setting: &str,
    excluded: &[String],
) -> Option<String> {
    let roots: Vec<String> = ir
        .steps
        .iter()
        .filter(|s| s.needs.0.is_empty() && !excluded.contains(&s.id))
        .map(|s| s.id.clone())
        .collect();
    if roots.is_empty() {
        return None;
    }
    let id = gate_id(ir);
    for s in ir.steps.iter_mut().filter(|s| roots.contains(&s.id)) {
        s.needs.0.push(id.clone());
    }
    let mut gate: StepSpec =
        serde_json::from_value(serde_json::json!({ "id": id, "gate": "manual" }))
            .expect("a bare manual gate decodes");
    gate.gate_expires_after = Some(FORK_RUN_APPROVAL_GATE_EXPIRES_AFTER_S);
    gate.origin = vec![OriginEntry {
        kind: OriginKind::ForkRunApprovalGate,
        name: setting.to_string(),
        source: None,
        content_hash: String::new(),
    }];
    gate.environment_approval_gate = Some(false);
    ir.steps.insert(0, gate);
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_gate as ag;

    fn ir(yaml: &str) -> PipelineIr {
        scarab_pipeline::compile_yaml(yaml).expect("compiles")
    }

    fn step<'a>(ir: &'a PipelineIr, id: &str) -> &'a StepSpec {
        ir.steps.iter().find(|s| s.id == id).expect(id)
    }

    /// The gate comes first, has the documented shape, and every root that
    /// will start needs it; a root excluded by its `when:` is left alone.
    #[test]
    fn the_gate_fronts_every_root_that_starts() {
        let mut ir = ir(r#"
steps:
  - { id: build, image: busybox }
  - { id: lint, image: busybox }
  - { id: docs, image: busybox }
  - { id: test, image: busybox, needs: [build] }
"#);
        let id = add_fork_run_approval_gate(&mut ir, "fork_run_approval", &["docs".into()]);
        assert_eq!(id.as_deref(), Some(FORK_RUN_APPROVAL_GATE_ID));
        assert_eq!(ir.steps[0].id, FORK_RUN_APPROVAL_GATE_ID);
        let gate = step(&ir, FORK_RUN_APPROVAL_GATE_ID);
        assert_eq!(gate.gate.as_deref(), Some("manual"));
        assert!(gate.needs.0.is_empty());
        assert_eq!(gate.gate_expires_after, Some(259_200));
        assert_eq!(gate.environment_approval_gate, Some(false));
        assert_eq!(gate.origin.len(), 1);
        assert_eq!(gate.origin[0].kind, OriginKind::ForkRunApprovalGate);
        assert_eq!(gate.origin[0].name, "fork_run_approval");
        assert_eq!(gate.origin[0].source, None);
        assert_eq!(gate.origin[0].content_hash, "");
        assert!(gate.is_fork_run_approval_gate());
        assert!(!gate.counts_as_environment_approval_gate());
        assert_eq!(step(&ir, "build").needs.0, vec![FORK_RUN_APPROVAL_GATE_ID]);
        assert_eq!(step(&ir, "lint").needs.0, vec![FORK_RUN_APPROVAL_GATE_ID]);
        assert!(step(&ir, "docs").needs.0.is_empty(), "excluded");
        assert_eq!(step(&ir, "test").needs.0, vec!["build"]);
        assert!(scarab_pipeline::validate(&ir).is_ok(), "a valid DAG");
    }

    /// No root starts: no gate. A taken id gets a suffix.
    #[test]
    fn no_gate_without_a_root_and_the_id_never_collides() {
        let mut none = ir("steps:\n  - { id: build, image: busybox }\n");
        assert_eq!(
            add_fork_run_approval_gate(&mut none, "fork_run_approval", &["build".into()]),
            None
        );
        assert_eq!(none.steps.len(), 1);
        let mut taken = ir("steps:\n  - { id: fork-run-approval, image: busybox }\n");
        assert_eq!(
            add_fork_run_approval_gate(&mut taken, "fork_run_approval", &[]).as_deref(),
            Some("fork-run-approval-2")
        );
    }

    /// The blocker of the design audit: the gate never stands in for an
    /// Environment's approval gate, in either order of adding. An authority
    /// Step behind it alone still gets the Environment's gate, which then
    /// needs the fork gate; the fork gate is marked `false`; and the
    /// Environment's gate stays an Environment approval gate behind it.
    #[test]
    fn the_gate_and_an_environments_gate_never_stand_in_for_each_other() {
        let yaml = r#"
environment: prod
deploy: true
steps:
  - { id: build, image: busybox }
  - { id: ship, image: busybox, needs: [build] }
"#;
        // As Run creation does: the Environment first, then the fork gate.
        let mut ir = ir(yaml);
        let env = ag::add_environment_approval_gate(&mut ir, "prod", None, &[]).unwrap();
        ag::mark_environment_approval_gates(&mut ir);
        let fork = add_fork_run_approval_gate(&mut ir, "fork_run_approval", &[]).unwrap();
        assert_eq!(step(&ir, &env.id).needs.0, vec![fork.clone()]);
        assert_eq!(step(&ir, &env.id).environment_approval_gate, Some(true));
        assert_eq!(step(&ir, &fork).environment_approval_gate, Some(false));
        let upstream: Vec<&str> = ag::upstream_approval_gates(&ir, "ship")
            .iter()
            .map(|g| g.id.as_str())
            .collect();
        assert_eq!(upstream, vec![env.id.as_str()]);
        assert!(ag::steps_missing_approval_gate(&ir, &[]).is_empty());

        // The other order: the fork gate alone in front of an authority Step
        // leaves it missing its Environment gate.
        let mut ir = self::ir(yaml);
        add_fork_run_approval_gate(&mut ir, "fork_run_approval", &[]).unwrap();
        assert_eq!(
            ag::steps_missing_approval_gate(&ir, &[]),
            vec!["build".to_string(), "ship".to_string()]
        );
        assert!(!ag::manual_gate_upstream(&ir, "build"));
        assert!(ag::upstream_approval_gates(&ir, "build").is_empty());
        assert!(ag::environment_approval_gates(&ir).is_empty());
    }
}
