//! The merge cases in `fixtures/effective_protection_cases.json` are the
//! server's: each case's `effective` is what `effective_protection` makes of
//! its `baseline` and `overlay`, as `GET /v1/repos/{org}/{repo}/environments`
//! serialises it. The UI mock's copy of the merge is checked against the same
//! file (`ui/scarab-web-ui/src/mock.test.ts`), so the two cannot drift apart
//! without one of the suites failing.

use scarab_project::{
    effective_protection, ApprovalGateRules, EnvironmentProjectOverlay, EnvironmentProtectionRules,
    DEFAULT_MAX_APPROVAL_AGE_S,
};
use scarab_server::org_environments::{
    EffectiveProtectionDto, PutEnvironmentProjectOverlayRequest, PutEnvironmentProtectionRequest,
};
use serde_json::Value;

const CASES: &str = include_str!("fixtures/effective_protection_cases.json");

/// A baseline in the shape `GET` returns `protection`, as the domain rules.
fn baseline(json: &Value) -> EnvironmentProtectionRules {
    let b: PutEnvironmentProtectionRequest =
        serde_json::from_value(json.clone()).expect("a baseline in the GET shape");
    EnvironmentProtectionRules {
        approvers: b.approvers,
        allowed_refs: b.allowed_refs,
        wait_timer_s: b.wait_timer_s,
        require_reason: b.require_reason,
        approval_gate: ApprovalGateRules {
            missing_gate: b.approval_gate.missing_gate_mode.into(),
            added_gate_expiry_s: b.approval_gate.added_gate_expiry_s,
            required_approvals: b.approval_gate.required_approvals,
            max_approval_age_s: b
                .approval_gate
                .max_approval_age_s
                .unwrap_or(DEFAULT_MAX_APPROVAL_AGE_S),
        },
    }
}

/// An overlay in its wire shape, as the domain overlay; `null` is none.
fn overlay(json: &Value) -> Option<EnvironmentProjectOverlay> {
    if json.is_null() {
        return None;
    }
    let o: PutEnvironmentProjectOverlayRequest =
        serde_json::from_value(json.clone()).expect("an overlay in its wire shape");
    Some(EnvironmentProjectOverlay {
        org: "acme".into(),
        environment: "prod".into(),
        project: "web".into(),
        rules: EnvironmentProtectionRules {
            approvers: o.approvers,
            allowed_refs: o.allowed_refs,
            wait_timer_s: o.wait_timer_s,
            require_reason: o.require_reason,
            ..Default::default()
        },
    })
}

#[test]
fn the_server_merge_gives_every_fixture_case_its_effective_rules() {
    let file: Value = serde_json::from_str(CASES).expect("the fixture is JSON");
    let cases = file["cases"].as_array().expect("a `cases` array");
    assert!(!cases.is_empty(), "the fixture has cases");
    for case in cases {
        let name = case["name"].as_str().expect("every case is named");
        let merged = effective_protection(
            &baseline(&case["baseline"]),
            overlay(&case["overlay"]).as_ref(),
        );
        let got = serde_json::to_value(EffectiveProtectionDto::from(&merged)).unwrap();
        assert_eq!(got, case["effective"], "case `{name}`");
    }
}
