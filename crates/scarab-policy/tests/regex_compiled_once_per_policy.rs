//! Compiling a Policy compiles each distinct `matches()` pattern once, not
//! once per rule that uses it, and stops compiling at the Policy's cap. A
//! Unicode `\w{20}` takes milliseconds to compile; 32 rules each using the
//! same 8 such patterns compiled 256 of them. The count is process-wide, so
//! this binary holds one test.

use scarab_policy::check::MAX_PATTERNS;
use scarab_policy::eval::regex_compiles;
use scarab_policy::ir::{compile, Level, MAX_POLICY_PATTERNS, MAX_RULES};
use scarab_policy::policy::Policy;
use serde_json::json;

fn rule(patterns: std::ops::Range<usize>) -> serde_json::Value {
    let when = patterns
        .map(|i| format!("pipeline.name.matches(\"\\\\w{{20}}{i}\")"))
        .collect::<Vec<_>>()
        .join(" || ");
    json!({ "when": when, "effect": "deny", "message": "m" })
}

fn policy(rules: Vec<serde_json::Value>) -> Policy {
    Policy::from_json(json!({
        "name": "names", "checkpoint": "run.create", "mode": "enforce", "rules": rules
    }))
    .unwrap()
}

#[test]
fn a_policy_compiles_each_distinct_pattern_once_and_stops_at_its_cap() {
    // 32 rules, each using the same 8 patterns: 8 compiles.
    let same = policy((0..MAX_RULES).map(|_| rule(0..MAX_PATTERNS)).collect());
    let before = regex_compiles();
    compile(&same, Level::Org).expect("8 distinct patterns fit");
    assert_eq!(regex_compiles() - before, MAX_PATTERNS as u64);

    // 32 rules of 8 different patterns each, 256 distinct: refused, having
    // compiled no more than the Policy's cap.
    let distinct = policy(
        (0..MAX_RULES)
            .map(|r| rule(r * MAX_PATTERNS..(r + 1) * MAX_PATTERNS))
            .collect(),
    );
    let before = regex_compiles();
    let errs = compile(&distinct, Level::Org).unwrap_err();
    let compiled = regex_compiles() - before;
    assert!(
        compiled <= MAX_POLICY_PATTERNS as u64,
        "{compiled} patterns compiled"
    );
    assert!(
        errs.iter().any(|d| d.at == "rules[4].when"
            && d.message
                .contains("a Policy uses at most 32 distinct matches() patterns")),
        "{errs:?}"
    );
}
