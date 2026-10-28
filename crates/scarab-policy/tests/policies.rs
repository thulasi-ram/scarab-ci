//! Realistic `invoke.resolve` policies: the ones that must check and decide as
//! expected, and the ones the checker must refuse with a useful message.

use scarab_policy::check::{check, MAX_COST};
use scarab_policy::eval::{compile_rule, evaluate_rule, RuleOutcome};
use scarab_policy::invoke_resolve::{
    schema, Environment, Invoke, InvokeResolveInput, InvokeTarget, Project, TargetKind,
};
use scarab_policy::schema::{Capped, Schema, Timestamp, Type};

struct Sample {
    kind: TargetKind,
    name: &'static str,
    version: Option<&'static str>,
    tags: &'static [&'static str],
    project: &'static str,
    environment: Option<&'static str>,
    time: &'static str,
}

const BASE: Sample = Sample {
    kind: TargetKind::Library,
    name: "scarab/rust",
    version: Some("1.4.0"),
    tags: &["owner:platform", "stable"],
    project: "payments-api",
    environment: Some("production"),
    time: "2026-11-02T10:00:00Z",
};

impl Sample {
    fn input(&self) -> InvokeResolveInput {
        InvokeResolveInput {
            invoke: Invoke {
                target: InvokeTarget {
                    kind: self.kind,
                    name: self.name.to_string(),
                    content_hash: "ab".repeat(32),
                    version: self.version.map(str::to_string),
                    tags: Capped::new(self.tags.iter().map(|t| t.to_string()).collect())
                        .expect("within the cap"),
                },
            },
            project: Project {
                name: self.project.to_string(),
            },
            environment: self.environment.map(|name| Environment {
                name: name.to_string(),
            }),
            time: Timestamp(
                chrono::DateTime::parse_from_rfc3339(self.time)
                    .expect("valid time")
                    .to_utc(),
            ),
        }
    }
}

fn applies(schema: &Schema, when: &str, sample: &Sample) -> bool {
    let rule = compile_rule(schema, when, "denied", None)
        .unwrap_or_else(|e| panic!("`{when}` should check: {e:?}"));
    let warnings: Vec<_> = rule.warnings().collect();
    assert!(
        warnings.is_empty(),
        "`{when}`: unexpected warnings {warnings:?}"
    );
    match evaluate_rule(schema, &rule, &sample.input()) {
        RuleOutcome::Applies { .. } => true,
        RuleOutcome::DoesNotApply => false,
        RuleOutcome::DeniedOnError { error, .. } => panic!("`{when}` failed: {error}"),
    }
}

// ---------------------------------------------------------------------------
// Policies that check and decide.
// ---------------------------------------------------------------------------

#[test]
fn library_allowlist_by_prefix_and_list() {
    let s = schema();
    let when = r#"invoke.target.kind == "library" &&
        !(invoke.target.name.startsWith("scarab/") ||
          invoke.target.name in ["acme/deploy-helm"])"#;
    assert!(!applies(&s, when, &BASE));
    assert!(!applies(
        &s,
        when,
        &Sample {
            name: "acme/deploy-helm",
            ..BASE
        }
    ));
    assert!(applies(
        &s,
        when,
        &Sample {
            name: "evil/miner",
            ..BASE
        }
    ));
    assert!(!applies(
        &s,
        when,
        &Sample {
            name: "evil/miner",
            kind: TargetKind::Local,
            ..BASE
        }
    ));
}

#[test]
fn every_plugin_requires_approval() {
    let s = schema();
    let when = r#"invoke.target.kind == "plugin""#;
    assert!(applies(
        &s,
        when,
        &Sample {
            kind: TargetKind::Plugin,
            ..BASE
        }
    ));
    assert!(!applies(&s, when, &BASE));
}

#[test]
fn holiday_freeze_window_in_production() {
    let s = schema();
    let when = r#"environment != null && environment.name == "production" &&
        time >= timestamp("2026-12-20T00:00:00Z") && time < timestamp("2027-01-04T00:00:00Z")"#;
    assert!(applies(
        &s,
        when,
        &Sample {
            time: "2026-12-24T12:00:00Z",
            ..BASE
        }
    ));
    assert!(!applies(&s, when, &BASE));
    assert!(!applies(
        &s,
        when,
        &Sample {
            time: "2026-12-24T12:00:00Z",
            environment: Some("staging"),
            ..BASE
        }
    ));
    // No Environment: the guard short-circuits instead of failing closed.
    assert!(!applies(
        &s,
        when,
        &Sample {
            time: "2026-12-24T12:00:00Z",
            environment: None,
            ..BASE
        }
    ));
}

#[test]
fn deprecated_tag_blocks_with_exists() {
    let s = schema();
    let when = r#"invoke.target.tags.exists(t, t == "deprecated")"#;
    assert!(applies(
        &s,
        when,
        &Sample {
            tags: &["stable", "deprecated"],
            ..BASE
        }
    ));
    assert!(!applies(&s, when, &BASE));
    assert!(!applies(&s, when, &Sample { tags: &[], ..BASE }));
}

#[test]
fn every_tag_must_be_well_formed_with_all_and_matches() {
    let s = schema();
    let when = r#"!invoke.target.tags.all(t, t.matches("^[a-z0-9:-]+$"))"#;
    assert!(!applies(&s, when, &BASE));
    assert!(applies(
        &s,
        when,
        &Sample {
            tags: &["stable", "Owner:X"],
            ..BASE
        }
    ));
}

#[test]
fn exactly_one_owner_with_exists_one() {
    let s = schema();
    let when = r#"!invoke.target.tags.exists_one(t, t.startsWith("owner:"))"#;
    assert!(!applies(&s, when, &BASE));
    assert!(applies(
        &s,
        when,
        &Sample {
            tags: &["stable"],
            ..BASE
        }
    ));
    assert!(applies(
        &s,
        when,
        &Sample {
            tags: &["owner:a", "owner:b"],
            ..BASE
        }
    ));
}

#[test]
fn unpinned_or_prerelease_library_with_has() {
    let s = schema();
    let when = r#"invoke.target.kind == "library" &&
        (!has(invoke.target.version) || invoke.target.version.endsWith("-rc"))"#;
    assert!(!applies(&s, when, &BASE));
    assert!(applies(
        &s,
        when,
        &Sample {
            version: None,
            ..BASE
        }
    ));
    assert!(applies(
        &s,
        when,
        &Sample {
            version: Some("2.0.0-rc"),
            ..BASE
        }
    ));
}

#[test]
fn content_hash_must_be_a_sha256() {
    let s = schema();
    let when = r#"size(invoke.target.content_hash) != 64 ||
        !invoke.target.content_hash.matches("^[0-9a-f]+$")"#;
    assert!(!applies(&s, when, &BASE));
}

#[test]
fn payments_projects_get_one_plugin() {
    let s = schema();
    let when = r#"project.name.startsWith("payments-") && invoke.target.kind == "plugin" &&
        !(invoke.target.name in ["scarab/argocd"])"#;
    let plugin = Sample {
        kind: TargetKind::Plugin,
        ..BASE
    };
    assert!(applies(
        &s,
        when,
        &Sample {
            name: "acme/helm",
            ..plugin
        }
    ));
    assert!(!applies(
        &s,
        when,
        &Sample {
            name: "scarab/argocd",
            ..plugin
        }
    ));
    assert!(!applies(
        &s,
        when,
        &Sample {
            name: "acme/helm",
            project: "web",
            ..plugin
        }
    ));
}

#[test]
fn no_local_invokes_in_production_with_a_conditional() {
    let s = schema();
    let when = r#"environment != null
        ? environment.name == "production" && invoke.target.kind == "local"
        : false"#;
    assert!(applies(
        &s,
        when,
        &Sample {
            kind: TargetKind::Local,
            ..BASE
        }
    ));
    assert!(!applies(
        &s,
        when,
        &Sample {
            kind: TargetKind::Local,
            environment: None,
            ..BASE
        }
    ));
    assert!(!applies(&s, when, &BASE));
}

#[test]
fn blocklist_as_a_map_literal() {
    let s = schema();
    let when = r#"invoke.target.name in {"evil/miner": true, "bad/lib": true}"#;
    assert!(applies(
        &s,
        when,
        &Sample {
            name: "bad/lib",
            ..BASE
        }
    ));
    assert!(!applies(&s, when, &BASE));
}

#[test]
fn path_traversal_in_a_name() {
    let s = schema();
    let when = r#"invoke.target.name.contains("..") || invoke.target.name.endsWith("/")"#;
    assert!(applies(
        &s,
        when,
        &Sample {
            name: "scarab/../x",
            ..BASE
        }
    ));
    assert!(!applies(&s, when, &BASE));
}

#[test]
fn duration_literals_compare() {
    let s = schema();
    assert!(applies(&s, r#"duration("1h30m") > duration("1h")"#, &BASE));
}

#[test]
fn message_expr_is_a_checked_string() {
    let s = schema();
    let rule = compile_rule(
        &s,
        r#"!invoke.target.name.startsWith("scarab/")"#,
        "This library is not approved.",
        Some(r#""not approved: " + invoke.target.name"#),
    )
    .expect("checks");
    assert_eq!(
        evaluate_rule(
            &s,
            &rule,
            &Sample {
                name: "evil/miner",
                ..BASE
            }
            .input()
        ),
        RuleOutcome::Applies {
            message: "not approved: evil/miner".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// Optional reads: a warning when checked, a deny when evaluated.
// ---------------------------------------------------------------------------

#[test]
fn unguarded_optional_field_warns_and_fails_closed() {
    let s = schema();
    let rule = compile_rule(
        &s,
        r#"invoke.target.version.startsWith("0.")"#,
        "denied",
        None,
    )
    .expect("a warning, not an error");
    let warnings: Vec<String> = rule.warnings().map(|w| w.to_string()).collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("guard the read with `has(invoke.target.version) && ...`"),
        "{warnings:?}"
    );
    assert_eq!(
        evaluate_rule(&s, &rule, &BASE.input()),
        RuleOutcome::DoesNotApply
    );
    let RuleOutcome::DeniedOnError { error, .. } = evaluate_rule(
        &s,
        &rule,
        &Sample {
            version: None,
            ..BASE
        }
        .input(),
    ) else {
        panic!("a missing optional field must deny");
    };
    assert!(error.contains("version"), "{error}");
}

#[test]
fn unguarded_optional_variable_warns_and_fails_closed() {
    let s = schema();
    let rule = compile_rule(&s, r#"environment.name == "production""#, "denied", None)
        .expect("a warning, not an error");
    let warnings: Vec<String> = rule.warnings().map(|w| w.to_string()).collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("`environment` may be null"),
        "{warnings:?}"
    );
    assert!(matches!(
        evaluate_rule(
            &s,
            &rule,
            &Sample {
                environment: None,
                ..BASE
            }
            .input()
        ),
        RuleOutcome::DeniedOnError { .. }
    ));
}

// ---------------------------------------------------------------------------
// Policies the checker refuses.
// ---------------------------------------------------------------------------

/// Checks `when` as a bool rule and returns its first error, rendered.
fn rejected(when: &str) -> String {
    match check(&schema(), when, &Type::Bool) {
        Ok(c) => panic!("`{when}` should be rejected, checked as {:?}", c.result),
        Err(errors) => errors[0].to_string(),
    }
}

fn assert_rejected(when: &str, expected: &str) {
    let error = rejected(when);
    assert!(
        error.contains(expected),
        "`{when}`\n  got:      {error}\n  expected: …{expected}…"
    );
}

#[test]
fn misspelt_field() {
    assert_eq!(
        rejected(r#"invoke.target.nmae == "x""#),
        "1:14: `invoke.target.nmae`: no field `nmae` on InvokeTarget (did you mean `name`?); \
         fields are kind, name, content_hash, version, tags"
    );
}

#[test]
fn misspelt_variable() {
    assert_rejected(
        r#"invokee.target.name == "x""#,
        "`invokee`: no variable `invokee` at invoke.resolve (did you mean `invoke`?); \
         variables are invoke, project, environment, time",
    );
}

#[test]
fn misspelt_enum_value() {
    assert_rejected(
        r#"invoke.target.kind == "libary""#,
        r#"`"libary"`: "libary" is not a TargetKind (did you mean `library`?); values are local, library, plugin"#,
    );
    assert_rejected(
        r#"invoke.target.kind in ["local", "plugn"]"#,
        r#""plugn" is not a TargetKind"#,
    );
}

#[test]
fn string_compared_with_int() {
    assert_rejected(
        r#"invoke.target.name == 1"#,
        "`invoke.target.name == 1`: cannot compare string with int",
    );
}

#[test]
fn timestamp_ordered_against_a_string() {
    assert_rejected(
        r#"time > "2026-01-01T00:00:00Z""#,
        "cannot order timestamp against string",
    );
}

#[test]
fn when_that_is_not_a_bool() {
    assert_rejected(
        "invoke.target.name",
        "the rule must return bool, but this returns string",
    );
}

#[test]
fn message_expr_that_is_not_a_string() {
    let err = compile_rule(&schema(), "true", "m", Some("size(invoke.target.tags)"))
        .expect_err("an int message");
    assert!(
        err[0]
            .message
            .contains("must return string, but this returns int"),
        "{err:?}"
    );
}

#[test]
fn unsupported_function_and_operator() {
    assert_rejected(
        r#"invoke.target.name.lowerAscii() == "x""#,
        "the function `lowerAscii` is not supported in policies",
    );
    assert_rejected(
        "size(invoke.target.tags) * 2 > 4",
        "the `*` operator is not supported in policies",
    );
    assert_rejected(
        "invoke.target.tags[0] == \"x\"",
        "the `[]` operator is not supported in policies",
    );
}

#[test]
fn non_literal_and_invalid_regex() {
    assert_rejected(
        "invoke.target.name.matches(project.name)",
        "`project.name`: matches() takes a string literal pattern in policies",
    );
    assert_rejected(r#"invoke.target.name.matches("[")"#, "invalid pattern");
}

#[test]
fn macro_over_a_non_list() {
    assert_rejected(
        r#"invoke.target.name.exists(c, c == "a")"#,
        "`invoke.target.name`: `exists` needs a list, not string",
    );
    assert_rejected(
        "invoke.target.tags.map(t, t).size() > 0",
        "the `map/filter` macro is not supported in policies",
    );
}

#[test]
fn iteration_variable_has_the_element_type() {
    assert_rejected(
        "invoke.target.tags.exists(t, t > 3)",
        "`t > 3`: cannot order string against int",
    );
    assert_rejected(
        "invoke.target.tags.all(t, t.name == \"x\")",
        "`t.name`: cannot select a field from string",
    );
}

#[test]
fn cost_above_the_limit() {
    let error = rejected(
        "invoke.target.tags.exists(a, invoke.target.tags.exists(b, \
         invoke.target.tags.exists(c, a == b && b == c)))",
    );
    assert!(
        error.contains(&format!("above the limit of {MAX_COST}")),
        "{error}"
    );
    // Two levels fit.
    check(
        &schema(),
        "invoke.target.tags.exists(a, invoke.target.tags.exists(b, a == b))",
        &Type::Bool,
    )
    .expect("two nested macros fit");
}

#[test]
fn absent_field_is_not_null() {
    assert_rejected(
        "invoke.target.version == null",
        "invoke.target.version is string and is never null; test presence with has(invoke.target.version)",
    );
}

#[test]
fn has_on_a_misspelt_field_and_bad_literals() {
    assert_rejected(
        "has(invoke.target.verison)",
        "no field `verison` on InvokeTarget (did you mean `version`?)",
    );
    assert_rejected(
        r#"time > timestamp("yesterday")"#,
        "not a valid timestamp: yesterday",
    );
    assert_rejected("does not parse (", "does not parse");
}

#[test]
fn oversized_and_overnested_rules() {
    let long = format!("invoke.target.name == \"{}\"", "x".repeat(5000));
    assert_rejected(&long, "a rule is at most 4096 bytes");
    // A left-recursive chain, not `((((…`: in a debug build cel 0.15's parser
    // overflows a test thread's 2 MiB stack at about 12 nested parentheses.
    let deep = format!("{} == \"x\"", vec!["\"a\""; 70].join(" + "));
    assert_rejected(&deep, "nested more than 64 deep");
}
