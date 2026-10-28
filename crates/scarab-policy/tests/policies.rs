//! Realistic `run.create` policies: the ones that must check and decide as
//! expected, and the ones the checker must refuse with a useful message.

use scarab_policy::check::{check, Diagnostic, MAX_COST};
use scarab_policy::evaluate::{evaluate, MatchContext, PolicyOutcome};
use scarab_policy::ir::{compile, Level, PolicyIr};
use scarab_policy::policy::{Checkpoint, Policy};
use scarab_policy::run_create::{
    schema, ForgeToken, Gate, GateKind, Org, Origin, OriginKind, Pipeline, Project, RunCreateInput,
    Step, Trigger, TriggerKind, UpstreamGate,
};
use scarab_policy::schema::{Capped, Schema, Timestamp, Type};

fn cap<T, const N: usize>(items: Vec<T>) -> Capped<T, N> {
    Capped::new(items).expect("within the cap")
}

fn strings<const N: usize>(items: &[&str]) -> Capped<String, N> {
    cap(items.iter().map(|s| s.to_string()).collect())
}

fn at(time: &str) -> Timestamp {
    Timestamp(
        chrono::DateTime::parse_from_rfc3339(time)
            .expect("valid time")
            .to_utc(),
    )
}

fn library(name: &str, source: &str) -> Origin {
    Origin {
        kind: OriginKind::Library,
        name: name.to_string(),
        source: Some(source.to_string()),
        content_hash: "ab".repeat(32),
    }
}

fn origin(kind: OriginKind, name: &str) -> Origin {
    Origin {
        kind,
        name: name.to_string(),
        source: None,
        content_hash: "cd".repeat(32),
    }
}

fn step(id: &str) -> Step {
    Step {
        id: id.to_string(),
        image: "registry.acme.dev/rust:1.84".to_string(),
        digest: Some(format!("sha256:{}", "e".repeat(64))),
        grants: strings(&[]),
        k8s_overlay: false,
        secrets: strings(&[]),
        placement_profiles: strings(&[]),
        deploy: false,
        forge_token: Some(ForgeToken::Read),
        uses_environment_authority: false,
        origin: cap(vec![]),
        upstream_gates: cap(vec![]),
    }
}

/// A push to main of `payments-api`'s production Pipeline: a test Step, a
/// manual gate, and a deploy Step from an approved library behind it.
fn base() -> RunCreateInput {
    let deploy = Step {
        deploy: true,
        uses_environment_authority: true,
        secrets: strings(&["KUBECONFIG"]),
        origin: cap(vec![library(
            "deploy-helm",
            "github.com/acme/scarab-libs//deploy-helm.yaml",
        )]),
        upstream_gates: cap(vec![UpstreamGate {
            id: "approve".to_string(),
            kind: GateKind::Manual,
        }]),
        ..step("deploy/helm")
    };
    RunCreateInput {
        org: Org {
            name: "acme".to_string(),
        },
        project: Project {
            name: "payments-api".to_string(),
        },
        trigger: Trigger {
            kind: TriggerKind::Push,
            r#ref: Some("refs/heads/main".to_string()),
            sha: Some("f".repeat(40)),
            fork: false,
            actor: Some("octocat".to_string()),
            principal: Some("user:octocat".to_string()),
            reason: None,
        },
        pipeline: Pipeline {
            name: "release".to_string(),
            environment: Some("production".to_string()),
            deploy: false,
        },
        steps: cap(vec![step("test"), deploy]),
        gates: cap(vec![Gate {
            id: "approve".to_string(),
            kind: GateKind::Manual,
            origin: cap(vec![]),
            dependents: strings(&["deploy/helm"]),
        }]),
        principal: None,
        time: at("2026-11-02T10:00:00Z"),
    }
}

/// `base()` with the deploy Step's invoke chain replaced.
fn deploying_from(chain: Vec<Origin>) -> RunCreateInput {
    let mut input = base();
    let mut steps: Vec<Step> = vec![step("test")];
    steps.push(Step {
        origin: cap(chain),
        ..step("deploy/helm")
    });
    input.steps = cap(steps);
    input
}

fn with_steps(steps: Vec<Step>) -> RunCreateInput {
    RunCreateInput {
        steps: cap(steps),
        ..base()
    }
}

// ---------------------------------------------------------------------------
// One rule through the production path: a one-rule enforced org Policy,
// `ir::compile`d and `evaluate::evaluate`d against the input as JSON.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum RuleOutcome {
    /// `when` was false.
    DoesNotApply,
    /// `when` was true: the Policy denied with this message.
    Applies { message: String },
    /// Evaluation failed, so the Policy denied.
    DeniedOnError { error: String },
}

struct Rule {
    ir: PolicyIr,
}

impl Rule {
    fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.ir.warnings()
    }
}

/// `schema` is `run_create::schema()`, the one `compile` checks against.
fn compile_rule(schema: &Schema, when: &str, message: &str) -> Result<Rule, Vec<Diagnostic>> {
    assert_eq!(schema.checkpoint, "run.create");
    let policy = Policy::from_json(serde_json::json!({
        "name": "rule",
        "checkpoint": "run.create",
        "mode": "enforce",
        "rules": [{ "when": when, "effect": "deny", "message": message }]
    }))
    .expect("a valid Policy");
    compile(&policy, Level::Org).map(|ir| Rule { ir })
}

fn evaluate_rule(_schema: &Schema, rule: &Rule, input: &RunCreateInput) -> RuleOutcome {
    let ctx = MatchContext {
        project: input.project.name.clone(),
        environment: input.pipeline.environment.clone(),
        trigger_kind: input.trigger.kind,
    };
    let input = serde_json::to_value(input).expect("serialises");
    let d = evaluate(
        Checkpoint::RunCreate,
        scarab_policy::run_create::SCHEMA_VERSION,
        &input,
        &ctx,
        std::slice::from_ref(&rule.ir),
    );
    match &d.results[0].outcome {
        PolicyOutcome::Passed => RuleOutcome::DoesNotApply,
        PolicyOutcome::Denied { messages } => RuleOutcome::Applies {
            message: messages.join("; "),
        },
        PolicyOutcome::Error { message } => RuleOutcome::DeniedOnError {
            error: message.clone(),
        },
        other => panic!("an enforced, matched Policy: {other:?}"),
    }
}

fn outcome(schema: &Schema, when: &str, input: &RunCreateInput) -> RuleOutcome {
    let rule = compile_rule(schema, when, "denied")
        .unwrap_or_else(|e| panic!("`{when}` should check: {e:?}"));
    evaluate_rule(schema, &rule, input)
}

fn applies(schema: &Schema, when: &str, input: &RunCreateInput) -> bool {
    let rule = compile_rule(schema, when, "denied")
        .unwrap_or_else(|e| panic!("`{when}` should check: {e:?}"));
    let warnings: Vec<_> = rule.warnings().collect();
    assert!(
        warnings.is_empty(),
        "`{when}`: unexpected warnings {warnings:?}"
    );
    match evaluate_rule(schema, &rule, input) {
        RuleOutcome::Applies { .. } => true,
        RuleOutcome::DoesNotApply => false,
        RuleOutcome::DeniedOnError { error, .. } => panic!("`{when}` failed: {error}"),
    }
}

// ---------------------------------------------------------------------------
// ADR-0075 §4's example rule.
// ---------------------------------------------------------------------------

const ADR_EXAMPLE: &str = r#"steps.exists(s, s.origin.exists(o,
        o.kind == "library" &&
        !(o.source.startsWith("github.com/thulasi-ram/scarab-ci/") ||
          o.source in ["github.com/acme/scarab-libs//deploy-helm.yaml"])))"#;

#[test]
fn adr_example_checks_within_the_cost_bound_and_warns_on_source() {
    let s = schema();
    let checked = check(&s, ADR_EXAMPLE, &Type::Bool).expect("the ADR's example checks");
    // 1 + MAX_STEPS × (3 + 18 × MAX_ORIGIN), see run_create's module docs.
    assert_eq!(checked.cost, 75_265);
    assert!(checked.cost <= MAX_COST);
    // `source` is optional: `o.kind == "library"` does not tell the checker
    // it is present, so both reads draw the editor's warning.
    let warnings: Vec<String> = checked.warnings.iter().map(|w| w.to_string()).collect();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    for w in &warnings {
        assert!(
            w.contains("`source` is optional; guard the read with `has(o.source) && ...`"),
            "{w}"
        );
    }
}

#[test]
fn adr_example_decides() {
    let s = schema();
    let allowed = |chain: Vec<Origin>| outcome(&s, ADR_EXAMPLE, &deploying_from(chain));
    assert_eq!(outcome(&s, ADR_EXAMPLE, &base()), RuleOutcome::DoesNotApply);
    assert_eq!(
        allowed(vec![library(
            "rust",
            "github.com/thulasi-ram/scarab-ci/libs/rust.yaml"
        )]),
        RuleOutcome::DoesNotApply
    );
    assert!(matches!(
        allowed(vec![library("miner", "github.com/evil/libs//miner.yaml")]),
        RuleOutcome::Applies { .. }
    ));
    // Deeper in the chain counts too.
    assert!(matches!(
        allowed(vec![
            origin(OriginKind::File, ".scarab/lib/deploy.yaml"),
            library("miner", "github.com/evil/libs//miner.yaml"),
        ]),
        RuleOutcome::Applies { .. }
    ));
    // Files and plugins are not libraries.
    assert_eq!(
        allowed(vec![origin(OriginKind::Plugin, "argocd")]),
        RuleOutcome::DoesNotApply
    );
    // A library link without a source fails closed: the warning's point.
    assert!(matches!(
        allowed(vec![origin(OriginKind::Library, "nameless")]),
        RuleOutcome::DeniedOnError { .. }
    ));
}

#[test]
fn adr_example_with_has_guard_has_no_warnings() {
    let s = schema();
    let when = r#"steps.exists(s, s.origin.exists(o,
        o.kind == "library" && has(o.source) &&
        !(o.source.startsWith("github.com/thulasi-ram/scarab-ci/") ||
          o.source in ["github.com/acme/scarab-libs//deploy-helm.yaml"])))"#;
    assert!(!applies(&s, when, &base()));
    assert!(applies(
        &s,
        when,
        &deploying_from(vec![library("miner", "github.com/evil/libs//miner.yaml")])
    ));
}

// ---------------------------------------------------------------------------
// Policies that check and decide.
// ---------------------------------------------------------------------------

#[test]
fn every_plugin_gate_is_refused() {
    let s = schema();
    let when = r#"gates.exists(g, g.kind == "plugin")"#;
    assert!(!applies(&s, when, &base()));
    let mut input = base();
    input.gates = cap(vec![Gate {
        id: "argocd".to_string(),
        kind: GateKind::Plugin,
        origin: cap(vec![origin(OriginKind::Plugin, "argocd")]),
        dependents: strings(&[]),
    }]);
    assert!(applies(&s, when, &input));
}

#[test]
fn holiday_freeze_window_in_production() {
    let s = schema();
    let when = r#"has(pipeline.environment) && pipeline.environment == "production" &&
        time >= timestamp("2026-12-20T00:00:00Z") && time < timestamp("2027-01-04T00:00:00Z")"#;
    let christmas = RunCreateInput {
        time: at("2026-12-24T12:00:00Z"),
        ..base()
    };
    assert!(applies(&s, when, &christmas));
    assert!(!applies(&s, when, &base()));
    let mut staging = RunCreateInput {
        time: at("2026-12-24T12:00:00Z"),
        ..base()
    };
    staging.pipeline.environment = Some("staging".to_string());
    assert!(!applies(&s, when, &staging));
    // No Environment: the guard short-circuits instead of failing closed.
    let mut none = RunCreateInput {
        time: at("2026-12-24T12:00:00Z"),
        ..base()
    };
    none.pipeline.environment = None;
    assert!(!applies(&s, when, &none));
}

#[test]
fn privileged_grant_blocks_with_exists() {
    let s = schema();
    let when = r#"steps.exists(s, s.grants.exists(g, g == "privileged"))"#;
    assert!(!applies(&s, when, &base()));
    let privileged = Step {
        grants: strings(&["add-capabilities", "privileged"]),
        ..step("build")
    };
    assert!(applies(&s, when, &with_steps(vec![privileged])));
    assert!(!applies(&s, when, &with_steps(vec![])));
}

#[test]
fn every_image_from_the_registry_with_all_and_matches() {
    let s = schema();
    let when = r#"!steps.all(s, s.image.matches("^registry\\.acme\\.dev/"))"#;
    assert!(!applies(&s, when, &base()));
    let docker_hub = Step {
        image: "docker.io/library/alpine:3".to_string(),
        ..step("lint")
    };
    assert!(applies(
        &s,
        when,
        &with_steps(vec![step("test"), docker_hub])
    ));
}

#[test]
fn exactly_one_deploy_step_with_exists_one() {
    let s = schema();
    let when = "!steps.exists_one(s, s.deploy)";
    assert!(!applies(&s, when, &base()));
    assert!(applies(&s, when, &with_steps(vec![step("test")])));
    let two = vec![
        Step {
            deploy: true,
            ..step("a")
        },
        Step {
            deploy: true,
            ..step("b")
        },
    ];
    assert!(applies(&s, when, &with_steps(two)));
}

#[test]
fn unpinned_or_latest_image_with_has() {
    let s = schema();
    let when = r#"steps.exists(s, !has(s.digest) || s.image.endsWith(":latest"))"#;
    assert!(!applies(&s, when, &base()));
    let unpinned = Step {
        digest: None,
        ..step("build")
    };
    assert!(applies(&s, when, &with_steps(vec![unpinned])));
    let latest = Step {
        image: "registry.acme.dev/rust:latest".to_string(),
        ..step("build")
    };
    assert!(applies(&s, when, &with_steps(vec![latest])));
}

#[test]
fn content_hash_must_be_a_sha256() {
    let s = schema();
    let when = r#"steps.exists(s, s.origin.exists(o,
        size(o.content_hash) != 64 || !o.content_hash.matches("^[0-9a-f]+$")))"#;
    assert!(!applies(&s, when, &base()));
    let mut short = origin(OriginKind::File, ".scarab/lib/x.yaml");
    short.content_hash = "abc".to_string();
    assert!(applies(&s, when, &deploying_from(vec![short])));
}

#[test]
fn payments_projects_get_one_plugin() {
    let s = schema();
    let when = r#"project.name.startsWith("payments-") && gates.exists(g,
        g.origin.exists(o, o.kind == "plugin" && !(o.name in ["scarab/argocd"])))"#;
    let with_plugin = |project: &str, plugin: &str| {
        let mut input = base();
        input.project.name = project.to_string();
        input.gates = cap(vec![Gate {
            id: "sync".to_string(),
            kind: GateKind::Plugin,
            origin: cap(vec![origin(OriginKind::Plugin, plugin)]),
            dependents: strings(&[]),
        }]);
        input
    };
    assert!(applies(&s, when, &with_plugin("payments-api", "acme/helm")));
    assert!(!applies(
        &s,
        when,
        &with_plugin("payments-api", "scarab/argocd")
    ));
    assert!(!applies(&s, when, &with_plugin("web", "acme/helm")));
}

#[test]
fn no_local_files_in_production_with_a_conditional() {
    let s = schema();
    let when = r#"has(pipeline.environment)
        ? pipeline.environment == "production" &&
          steps.exists(s, s.origin.exists(o, o.kind == "file"))
        : false"#;
    let local = deploying_from(vec![origin(OriginKind::File, ".scarab/lib/deploy.yaml")]);
    assert!(applies(&s, when, &local));
    let mut no_env = local;
    no_env.pipeline.environment = None;
    assert!(!applies(&s, when, &no_env));
    assert!(!applies(&s, when, &base()));
}

#[test]
fn blocklist_as_a_map_literal() {
    let s = schema();
    let when = r#"steps.exists(s, s.image in {"evil/miner:1": true, "bad/image:2": true})"#;
    let bad = Step {
        image: "bad/image:2".to_string(),
        ..step("x")
    };
    assert!(applies(&s, when, &with_steps(vec![bad])));
    assert!(!applies(&s, when, &base()));
}

#[test]
fn path_traversal_in_an_origin_name() {
    let s = schema();
    let when = r#"steps.exists(s, s.origin.exists(o,
        o.name.contains("..") || o.name.endsWith("/")))"#;
    assert!(applies(
        &s,
        when,
        &deploying_from(vec![origin(OriginKind::File, ".scarab/../x.yaml")])
    ));
    assert!(!applies(&s, when, &base()));
}

#[test]
fn environment_authority_needs_a_manual_gate_upstream() {
    // ADR-0075 §5's deny: a Step using the Environment's authority with no
    // manual gate before it. It must fit the cost bound.
    let s = schema();
    let when = r#"steps.exists(s, s.uses_environment_authority &&
        !s.upstream_gates.exists(g, g.kind == "manual"))"#;
    assert_eq!(check(&s, when, &Type::Bool).expect("checks").cost, 85_505);
    assert!(!applies(&s, when, &base()));
    let ungated = Step {
        uses_environment_authority: true,
        secrets: strings(&["TOKEN"]),
        ..step("deploy")
    };
    assert!(applies(&s, when, &with_steps(vec![ungated])));
}

#[test]
fn manual_dispatch_needs_a_reason() {
    // ADR-0075 §5: a missing reason is a deny over `reason`.
    let s = schema();
    let when = r#"trigger.kind in ["manual", "api"] && !has(trigger.reason)"#;
    assert!(!applies(&s, when, &base()));
    let mut manual = base();
    manual.trigger.kind = TriggerKind::Manual;
    assert!(applies(&s, when, &manual));
    manual.trigger.reason = Some("hotfix".to_string());
    assert!(!applies(&s, when, &manual));
}

#[test]
fn duration_literals_compare() {
    let s = schema();
    assert!(applies(
        &s,
        r#"duration("1h30m") > duration("1h")"#,
        &base()
    ));
}

#[test]
fn rule_message_is_static() {
    let s = schema();
    let rule = compile_rule(
        &s,
        r#"trigger.fork"#,
        "Runs from forks are not allowed here.",
    )
    .expect("checks");
    let mut fork = base();
    fork.trigger.fork = true;
    assert_eq!(
        evaluate_rule(&s, &rule, &fork),
        RuleOutcome::Applies {
            message: "Runs from forks are not allowed here.".to_string()
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
        r#"trigger.ref.startsWith("refs/heads/release/")"#,
        "denied",
    )
    .expect("a warning, not an error");
    let warnings: Vec<String> = rule.warnings().map(|w| w.to_string()).collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("guard the read with `has(trigger.ref) && ...`"),
        "{warnings:?}"
    );
    assert_eq!(evaluate_rule(&s, &rule, &base()), RuleOutcome::DoesNotApply);
    let mut cron = base();
    cron.trigger.kind = TriggerKind::Cron;
    cron.trigger.r#ref = None;
    let RuleOutcome::DeniedOnError { error, .. } = evaluate_rule(&s, &rule, &cron) else {
        panic!("a missing optional field must deny");
    };
    assert!(error.contains("ref"), "{error}");
}

#[test]
fn unguarded_optional_variable_warns_and_fails_closed() {
    let s = schema();
    let rule = compile_rule(&s, r#"principal.startsWith("user:")"#, "denied")
        .expect("a warning, not an error");
    let warnings: Vec<String> = rule.warnings().map(|w| w.to_string()).collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("`principal` may be null"),
        "{warnings:?}"
    );
    assert!(matches!(
        evaluate_rule(&s, &rule, &base()),
        RuleOutcome::DeniedOnError { .. }
    ));
    // Equality with a null variable is false, not an error: no warning.
    assert!(!applies(&s, r#"principal == "user:octocat""#, &base()));
    assert!(applies(
        &s,
        r#"principal == "user:octocat""#,
        &RunCreateInput {
            principal: Some("user:octocat".to_string()),
            ..base()
        }
    ));
    // Guarded, it neither warns nor fails.
    assert!(!applies(
        &s,
        r#"principal != null && principal.startsWith("bot:")"#,
        &RunCreateInput {
            principal: Some("user:octocat".to_string()),
            ..base()
        }
    ));
    assert!(!applies(
        &s,
        r#"principal != null && principal.startsWith("bot:")"#,
        &base()
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
        rejected(r#"trigger.rfe == "x""#),
        "1:8: `trigger.rfe`: no field `rfe` on Trigger (did you mean `ref`?); \
         fields are kind, ref, sha, fork, actor, principal, reason"
    );
}

#[test]
fn misspelt_variable() {
    assert_rejected(
        r#"triger.kind == "push""#,
        "`triger`: no variable `triger` at run.create (did you mean `trigger`?); \
         variables are org, project, trigger, pipeline, steps, gates, principal, time",
    );
}

#[test]
fn misspelt_enum_value() {
    assert_rejected(
        r#"trigger.kind == "pul_request""#,
        r#"`"pul_request"`: "pul_request" is not a TriggerKind (did you mean `pull_request`?); values are push, pull_request, tag, release, comment, cron, manual, api, upstream"#,
    );
    assert_rejected(
        r#"steps.exists(s, s.origin.exists(o, o.kind in ["file", "libary"]))"#,
        r#""libary" is not a OriginKind"#,
    );
}

#[test]
fn string_compared_with_int() {
    assert_rejected(
        r#"pipeline.name == 1"#,
        "`pipeline.name == 1`: cannot compare string with int",
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
        "pipeline.name",
        "the rule must return bool, but this returns string",
    );
}

#[test]
fn string_concatenation_is_not_supported() {
    // String `+` existed for `message_expr`, which ADR-0075 removed.
    assert_rejected(
        r#"pipeline.name + "-prod" == "release-prod""#,
        "the `+` operator is not supported in policies",
    );
    assert_rejected(
        r#"steps.exists(s, s.id == pipeline.name + "/deploy")"#,
        "`pipeline.name + \"/deploy\"`: the `+` operator is not supported in policies",
    );
}

#[test]
fn unsupported_function_and_operator() {
    assert_rejected(
        r#"pipeline.name.lowerAscii() == "x""#,
        "the function `lowerAscii` is not supported in policies",
    );
    assert_rejected(
        "size(steps) * 2 > 4",
        "the `*` operator is not supported in policies",
    );
    assert_rejected(
        r#"steps[0].id == "x""#,
        "the `[]` operator is not supported in policies",
    );
}

#[test]
fn non_literal_and_invalid_regex() {
    assert_rejected(
        "pipeline.name.matches(project.name)",
        "`project.name`: matches() takes a string literal pattern in policies",
    );
    assert_rejected(r#"pipeline.name.matches("[")"#, "invalid pattern");
}

#[test]
fn macro_over_a_non_list() {
    assert_rejected(
        r#"pipeline.name.exists(c, c == "a")"#,
        "`pipeline.name`: `exists` needs a list, not string",
    );
    assert_rejected(
        "steps.map(s, s.id).size() > 0",
        "the `map/filter` macro is not supported in policies",
    );
}

#[test]
fn iteration_variable_has_the_element_type() {
    assert_rejected(
        "steps.exists(s, s.id > 3)",
        "`s.id > 3`: cannot order string against int",
    );
    assert_rejected(
        r#"steps.exists(s, s.secrets.all(x, x.name == "x"))"#,
        "`x.name`: cannot select a field from string",
    );
}

#[test]
fn cost_above_the_limit() {
    // Two macros nested over the Steps: quadratic in the Run.
    let error = rejected("steps.exists(a, steps.exists(b, a.id == b.id && a.image != b.image))");
    assert!(
        error.contains(&format!("above the limit of {MAX_COST}")),
        "{error}"
    );
    // One over the Steps and one over each Step's origin fits (the ADR's
    // example, above); so does one over the Steps and one over its secrets.
    check(
        &schema(),
        r#"steps.exists(s, s.secrets.exists(x, x == "PROD_DB"))"#,
        &Type::Bool,
    )
    .expect("steps × secrets fits");
}

#[test]
fn absent_field_is_not_null() {
    assert_rejected(
        "trigger.ref == null",
        "trigger.ref is string and is never null; test presence with has(trigger.ref)",
    );
}

#[test]
fn has_on_a_misspelt_field_and_bad_literals() {
    assert_rejected(
        "has(trigger.rfe)",
        "no field `rfe` on Trigger (did you mean `ref`?)",
    );
    assert_rejected(
        r#"time > timestamp("yesterday")"#,
        "not a valid timestamp: yesterday",
    );
    assert_rejected("does not parse (", "does not parse");
}

#[test]
fn oversized_and_overnested_rules() {
    let long = format!("pipeline.name == \"{}\"", "x".repeat(5000));
    assert_rejected(&long, "a rule is at most 4096 bytes");
    // A left-recursive chain, not `((((…`: in a debug build cel 0.15's parser
    // overflows a test thread's 2 MiB stack at about 12 nested parentheses.
    let deep = format!("pipeline{} == \"x\"", ".name".repeat(70));
    assert_rejected(&deep, "nested more than 64 deep");
}
