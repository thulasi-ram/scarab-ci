//! The cel-spec conformance cases that fall inside the policy subset, run
//! through the checker. Opt-in: needs the cases extracted from a cel-rust
//! checkout (see `just policy-conformance`), named by
//! `SCARAB_CEL_CONFORMANCE_CASES`. Panics when run without it.
//!
//! A case is inside the subset when it declares no variables, types or
//! container and the checker does not answer "not supported in policies".
//! For those:
//!
//! - a `typed_result` (incl. `check_only`) passes when the checker deduces the
//!   same type;
//! - a value passes when the checker accepts and deduces the value's kind;
//! - an expected evaluation error with checking enabled passes when the checker
//!   accepts (cel-go's checker accepted it; the error is a runtime one), and is
//!   counted apart when the checker rejects an invalid timestamp or duration
//!   literal (an error caught earlier than cel-go catches it);
//! - a case with `disable_check: true` has no expected check outcome. Those are
//!   counted apart, by whether this checker accepts them.

use std::collections::BTreeMap;

use scarab_policy::check::check;
use scarab_policy::policy_struct;
use scarab_policy::schema::{Schema, Type};

policy_struct! {
    struct NoInput {}
}

fn kind(t: &Type) -> &'static str {
    match t {
        Type::Bool => "bool",
        Type::Int => "int",
        Type::Uint => "uint",
        Type::Double => "double",
        Type::String | Type::Enum(_) => "string",
        Type::Bytes => "bytes",
        Type::Null => "null",
        Type::Timestamp => "timestamp",
        Type::Duration => "duration",
        Type::List { .. } => "list",
        Type::Map { .. } => "map",
        Type::Struct(_) => "object",
        Type::Dyn => "dyn",
    }
}

#[test]
#[ignore = "needs SCARAB_CEL_CONFORMANCE_CASES; run with `just policy-conformance`"]
fn cel_spec_cases_inside_the_subset() {
    // cel 0.15's parser needs more than a test thread's 2 MiB of stack in a
    // debug build for `parse/nest` (12 nested calls overflow it; a release
    // build stops at its depth limit of 96 instead).
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(run)
        .expect("thread")
        .join()
        .expect("the run passes");
}

fn run() {
    let path = std::env::var("SCARAB_CEL_CONFORMANCE_CASES")
        .expect("SCARAB_CEL_CONFORMANCE_CASES must name the extracted cases (JSON lines)");
    let text = std::fs::read_to_string(&path).expect("readable cases file");
    let schema = Schema::for_root::<NoInput>("conformance", 1);

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut outside: BTreeMap<String, usize> = BTreeMap::new();
    let mut unparsed = Vec::new();
    let mut failures = Vec::new();
    let mut check_only = (0, 0, 0); // pass, fail, outside the subset
    for line in text.lines() {
        let case: serde_json::Value = serde_json::from_str(line).expect("a JSON case");
        let s = |k: &str| case[k].as_str().unwrap_or_default().to_string();
        let flag = |k: &str| case[k].as_bool().unwrap_or(false);
        let id = format!("{}/{}/{}", s("file"), s("section"), s("name"));
        let expr = s("expr");
        let is_check_only = flag("check_only");
        if flag("type_env") || flag("bindings") || flag("container") {
            *counts
                .entry("skipped: declares variables, types or a container")
                .or_default() += 1;
            if is_check_only {
                check_only.2 += 1;
            }
            continue;
        }
        // Checked against no particular result type: the deduced type is
        // compared below. `Dyn` is assignable from anything.
        let outcome = check(&schema, &expr, &Type::Dyn);
        let result = s("result");
        let verdict: Result<(), String> = match &outcome {
            Err(d) if d[0].message.contains("not supported in policies") => {
                *counts.entry("skipped: outside the subset").or_default() += 1;
                let what = d[0].message.replace(" is not supported in policies", "");
                let what = what.split(" (").next().unwrap_or_default().to_string();
                *outside.entry(what).or_default() += 1;
                if is_check_only {
                    check_only.2 += 1;
                }
                continue;
            }
            Err(d) if d[0].message.starts_with("does not parse") => {
                *counts
                    .entry("skipped: cel 0.15 does not parse it")
                    .or_default() += 1;
                unparsed.push(format!("{id}: `{expr}`"));
                if is_check_only {
                    check_only.2 += 1;
                }
                continue;
            }
            _ if flag("disable_check") => {
                let key = if outcome.is_ok() {
                    "disable_check: accepted"
                } else {
                    "disable_check: rejected"
                };
                *counts.entry(key).or_default() += 1;
                continue;
            }
            Err(d) if result == "eval_error" && d[0].message.starts_with("not a valid") => {
                // An out-of-range timestamp or duration literal: cel-go fails
                // at evaluation, this checker already when the rule is saved.
                *counts
                    .entry("in subset: eval error caught when checked")
                    .or_default() += 1;
                continue;
            }
            Err(d) => Err(format!("rejected: {}", d[0])),
            Ok(c) => match result.as_str() {
                "typed" => {
                    let want = s("deduced_type");
                    let got = c.result.to_string();
                    if got == want {
                        Ok(())
                    } else {
                        Err(format!("deduced {got}, expected {want}"))
                    }
                }
                "value" => {
                    let want = s("value_kind");
                    let got = kind(&c.result);
                    if got == want {
                        Ok(())
                    } else {
                        Err(format!("deduced {got}, the value is {want}"))
                    }
                }
                "eval_error" => Ok(()),
                other => Err(format!("unhandled expected result {other}")),
            },
        };
        match verdict {
            Ok(()) => {
                *counts.entry("in subset: pass").or_default() += 1;
                if is_check_only {
                    check_only.0 += 1;
                }
            }
            Err(why) => {
                *counts.entry("in subset: fail").or_default() += 1;
                if is_check_only {
                    check_only.1 += 1;
                }
                failures.push(format!("{id}: `{expr}`: {why}"));
            }
        }
    }
    println!(
        "check_only: pass {}, fail {}, outside {}",
        check_only.0, check_only.1, check_only.2
    );
    for (k, v) in &counts {
        println!("{k}: {v}");
    }
    let mut outside: Vec<_> = outside.into_iter().collect();
    outside.sort_by_key(|o| std::cmp::Reverse(o.1));
    for (what, n) in outside.iter().take(25) {
        println!("  outside: {n:4} {what}");
    }
    for u in &unparsed {
        println!("UNPARSED {u}");
    }
    for f in &failures {
        println!("FAIL {f}");
    }
    assert!(
        failures.is_empty(),
        "{} cases inside the subset fail",
        failures.len()
    );
}
