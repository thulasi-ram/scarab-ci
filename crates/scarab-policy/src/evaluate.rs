//! The evaluator (ADR-0075 §7, §9): every Policy whose checkpoint and `match`
//! fit is evaluated; any enforced deny wins and every deny message is
//! reported; an error denies; audit-mode hits are recorded as "would deny"
//! and change nothing.
//!
//! The input is a `serde_json::Value`, the form a [`PolicyDecision`] stores,
//! so a stored decision's input can be evaluated again (the policy test API).
//! CEL and typed predicates both read that one value.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::check::on_cel_stack;
use crate::eval::{bind, evaluate_when};
use crate::ir::{schema_for, Level, PolicyIr, Predicate, TypedPredicate};
use crate::policy::{glob_matches, pattern_matches, sha256_hex, Checkpoint, Effect, Match, Mode};
use crate::run_create::TriggerKind;
use crate::store::StoredPolicy;

/// The most messages one typed predicate reports; the rest are counted.
pub const MAX_TYPED_MESSAGES: usize = 10;

/// What a Policy's `match` is evaluated against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchContext {
    pub project: String,
    /// The Run's Environment, when its Pipeline names one.
    pub environment: Option<String>,
    pub trigger_kind: TriggerKind,
}

/// Whether `m` selects a consultation in `ctx`. Empty lists match anything;
/// a Run with no Environment matches only an empty `environments`.
pub fn matches(m: &Match, ctx: &MatchContext) -> bool {
    let any = |globs: &[String], v: &str| globs.iter().any(|g| glob_matches(g, v));
    (m.projects.is_empty() || any(&m.projects, &ctx.project))
        && !any(&m.except_projects, &ctx.project)
        && (m.environments.is_empty()
            || ctx
                .environment
                .as_deref()
                .is_some_and(|e| any(&m.environments, e)))
        && (m.trigger_kinds.is_empty() || m.trigger_kinds.contains(&ctx.trigger_kind))
}

/// One Policy's part in a decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicyOutcome {
    /// The Policy's `match` did not select this consultation.
    NotMatched,
    /// Matched, and no rule's `when` held.
    Passed,
    /// An enforced Policy denied, with these messages.
    Denied { messages: Vec<String> },
    /// An audit-mode Policy would have denied; nothing was enforced.
    WouldDeny { messages: Vec<String> },
    /// A rule could not be evaluated. In `enforce` mode this denies.
    Error { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyResult {
    pub policy_id: String,
    pub version: u64,
    pub name: String,
    pub level: Level,
    pub mode: Mode,
    pub outcome: PolicyOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecisionOutcome {
    Allow,
    Deny { messages: Vec<String> },
}

/// One consultation of the engine (ADR-0075 §9).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub checkpoint: Checkpoint,
    pub schema_version: u32,
    /// The input itself, as evaluated.
    pub input: Value,
    pub policy_set_digest: String,
    /// One per Policy at this checkpoint, matched or not, in the order given.
    pub results: Vec<PolicyResult>,
    pub outcome: DecisionOutcome,
}

impl PolicyDecision {
    pub fn is_denied(&self) -> bool {
        matches!(self.outcome, DecisionOutcome::Deny { .. })
    }

    /// The deny messages; empty when allowed.
    pub fn messages(&self) -> &[String] {
        match &self.outcome {
            DecisionOutcome::Allow => &[],
            DecisionOutcome::Deny { messages } => messages,
        }
    }
}

/// The PolicySetDigest: SHA-256 over the sorted lines
/// `policy_id@version:content_hash` of every Policy at `checkpoint`, matched
/// or not, across layers.
pub fn policy_set_digest(checkpoint: Checkpoint, policies: &[PolicyIr]) -> String {
    let mut lines: Vec<String> = policies
        .iter()
        .filter(|p| p.checkpoint == checkpoint)
        .map(|p| format!("{}@{}:{}", p.policy_id, p.version, p.content_hash))
        .collect();
    lines.sort();
    sha256_hex(lines.join("\n").as_bytes())
}

/// Evaluates every Policy in `policies` at `checkpoint` against `input`.
/// Policies at other checkpoints are left out of the decision and the digest.
///
/// CEL runs on the large-stack thread; if that thread cannot run, every
/// matched Policy is an error, and the decision denies when one is enforced.
pub fn evaluate(
    checkpoint: Checkpoint,
    schema_version: u32,
    input: &Value,
    ctx: &MatchContext,
    policies: &[PolicyIr],
) -> PolicyDecision {
    let at: Vec<&PolicyIr> = policies
        .iter()
        .filter(|p| p.checkpoint == checkpoint)
        .collect();
    let results = on_cel_stack(|| evaluate_all(checkpoint, schema_version, input, ctx, &at))
        .unwrap_or_else(|e| {
            at.iter()
                .map(|p| {
                    let outcome = if matches(&p.match_, ctx) {
                        PolicyOutcome::Error {
                            message: format!("policy `{}` could not be evaluated: {e}", p.name),
                        }
                    } else {
                        PolicyOutcome::NotMatched
                    };
                    result_of(p, outcome)
                })
                .collect()
        });
    PolicyDecision {
        checkpoint,
        schema_version,
        input: input.clone(),
        policy_set_digest: policy_set_digest(checkpoint, policies),
        outcome: combine(&results),
        results,
    }
}

/// Any enforced deny or error denies, with every such message.
fn combine(results: &[PolicyResult]) -> DecisionOutcome {
    let mut messages = Vec::new();
    for r in results {
        if r.mode != Mode::Enforce {
            continue;
        }
        match &r.outcome {
            PolicyOutcome::Denied { messages: m } => messages.extend(m.iter().cloned()),
            PolicyOutcome::Error { message } => messages.push(message.clone()),
            _ => {}
        }
    }
    if messages.is_empty() {
        DecisionOutcome::Allow
    } else {
        DecisionOutcome::Deny { messages }
    }
}

/// Evaluates stored Policies at `checkpoint`, as admission does: each is
/// compiled for its scope's level first.
///
/// - A Policy that no longer compiles (a newer checker refusing what an
///   older one saved) is an error result, so when it is enforced
///   and its `match` fits, the decision denies: fail closed.
/// - `input` is `Err(reason)` when the input could not be built (a Run over
///   a list cap, [`CapExceeded`](crate::run_create::CapExceeded)): every
///   matched Policy is then an error with that reason, and the recorded
///   input is `null`.
///
/// The PolicySetDigest covers every stored Policy at the checkpoint,
/// compiled or not.
///
/// At `run.create` the built-in plugin default applies too
/// ([`plugins_closed_default`]): its result follows the stored Policies'.
pub fn evaluate_stored(
    checkpoint: Checkpoint,
    schema_version: u32,
    input: Result<&Value, &str>,
    ctx: &MatchContext,
    stored: &[StoredPolicy],
) -> PolicyDecision {
    let compiled: Vec<(&StoredPolicy, Result<PolicyIr, String>)> = stored
        .iter()
        .filter(|p| p.body.checkpoint == checkpoint)
        .map(|p| {
            let ir = p.compile().map_err(|ds| {
                let found: Vec<String> = ds.iter().map(|d| d.to_string()).collect();
                format!(
                    "version {} of this policy no longer compiles, so it denies: {}",
                    p.version,
                    found.join("; ")
                )
            });
            (p, ir)
        })
        .collect();
    let irs: Vec<PolicyIr> = compiled
        .iter()
        .filter_map(|(_, ir)| ir.as_ref().ok().cloned())
        .collect();
    let mut evaluated = match input {
        Ok(v) => evaluate(checkpoint, schema_version, v, ctx, &irs).results,
        Err(reason) => irs
            .iter()
            .map(|p| {
                let outcome = if matches(&p.match_, ctx) {
                    PolicyOutcome::Error {
                        message: reason.to_string(),
                    }
                } else {
                    PolicyOutcome::NotMatched
                };
                result_of(p, outcome)
            })
            .collect(),
    }
    .into_iter();

    let mut digest_set = Vec::with_capacity(compiled.len());
    let mut results = Vec::with_capacity(compiled.len());
    for (p, ir) in compiled {
        match ir {
            Ok(ir) => {
                results.push(evaluated.next().expect("one result per compiled policy"));
                digest_set.push(ir);
            }
            Err(message) => {
                // What the digest and the result need from a Policy that has
                // no IR: its identity, `match` and mode, from the stored row.
                let stub = PolicyIr {
                    policy_id: p.id.clone(),
                    version: p.version,
                    content_hash: p.content_hash.clone(),
                    name: p.name.clone(),
                    level: p.scope.level(),
                    checkpoint,
                    match_: p.body.match_.clone(),
                    mode: p.body.mode,
                    rules: Vec::new(),
                };
                let outcome = if matches(&stub.match_, ctx) {
                    PolicyOutcome::Error { message }
                } else {
                    PolicyOutcome::NotMatched
                };
                results.push(result_of(&stub, outcome));
                digest_set.push(stub);
            }
        }
    }
    if checkpoint == Checkpoint::RunCreate {
        if let Ok(input) = input {
            results.extend(plugins_closed_default(input, ctx, &digest_set));
        }
    }
    PolicyDecision {
        checkpoint,
        schema_version,
        input: input.cloned().unwrap_or(Value::Null),
        policy_set_digest: policy_set_digest(checkpoint, &digest_set),
        outcome: combine(&results),
        results,
    }
}

impl PolicyDecision {
    /// Whether any Policy's `match` selected this consultation.
    pub fn any_matched(&self) -> bool {
        self.results
            .iter()
            .any(|r| r.outcome != PolicyOutcome::NotMatched)
    }
}

fn result_of(p: &PolicyIr, outcome: PolicyOutcome) -> PolicyResult {
    PolicyResult {
        policy_id: p.policy_id.clone(),
        version: p.version,
        name: p.name.clone(),
        level: p.level,
        mode: p.mode,
        outcome,
    }
}

/// Runs on the [`on_cel_stack`] thread.
fn evaluate_all(
    checkpoint: Checkpoint,
    schema_version: u32,
    input: &Value,
    ctx: &MatchContext,
    policies: &[&PolicyIr],
) -> Vec<PolicyResult> {
    // Bound once, and only when a matched Policy has a CEL rule.
    let mut context: Option<Result<cel::Context<'static, 'static>, String>> = None;

    let mut results = Vec::with_capacity(policies.len());
    for p in policies {
        if !matches(&p.match_, ctx) {
            results.push(result_of(p, PolicyOutcome::NotMatched));
            continue;
        }
        let mut denies = Vec::new();
        let mut errors = Vec::new();
        for (i, rule) in p.rules.iter().enumerate() {
            if !checkpoint.allows(&rule.effect) || rule.effect != Effect::Deny {
                errors.push(format!(
                    "rule {i}: `{}` is not evaluated at {checkpoint}",
                    rule.effect.name()
                ));
                continue;
            }
            let hit = match &rule.predicate {
                Predicate::Cel(c) => cel_context(&mut context, checkpoint, schema_version, input)
                    .and_then(|cx| evaluate_when(&c.source, cx))
                    .map(|b| {
                        if b {
                            vec![rule.message.clone()]
                        } else {
                            vec![]
                        }
                    }),
                // The input is bound for a typed predicate too, so an input
                // that breaks the schema, a list over its cap included, fails
                // closed whichever frontend wrote the rule.
                Predicate::Typed(t) => cel_context(&mut context, checkpoint, schema_version, input)
                    .and_then(|_| typed(t, input)),
            };
            match hit {
                Ok(m) => denies.extend(m),
                Err(e) => errors.push(format!(
                    "{} (the rule could not be evaluated, so it denies: {e})",
                    rule.message
                )),
            }
        }
        let outcome = if !errors.is_empty() {
            errors.extend(denies);
            PolicyOutcome::Error {
                message: errors.join("; "),
            }
        } else if denies.is_empty() {
            PolicyOutcome::Passed
        } else if p.mode == Mode::Audit {
            PolicyOutcome::WouldDeny { messages: denies }
        } else {
            PolicyOutcome::Denied { messages: denies }
        };
        results.push(result_of(p, outcome));
    }
    results
}

/// The CEL context for `input`, bound on first use.
fn cel_context<'c>(
    slot: &'c mut Option<Result<cel::Context<'static, 'static>, String>>,
    checkpoint: Checkpoint,
    schema_version: u32,
    input: &Value,
) -> Result<&'c cel::Context<'static, 'static>, String> {
    slot.get_or_insert_with(|| {
        let schema = schema_for(checkpoint, schema_version)
            .ok_or_else(|| format!("no input schema for {checkpoint} version {schema_version}"))?;
        bind(&schema, input).map_err(|e| format!("input does not match the schema: {e}"))
    })
    .as_ref()
    .map_err(|e| e.clone())
}

/// The `policy_id` of the built-in plugin default's result.
pub const PLUGINS_DEFAULT_ID: &str = "builtin:plugins_allowed";

/// ADR-0075 §7's closed default for plugins: no plugin's gates run until an
/// org admin lists the plugin. A gate whose origin includes a `plugin` link
/// is denied unless an org-level, enforced `plugins_allowed` setting whose
/// `match` fits lists that installation. A Project's setting, or one in
/// audit mode, lists nothing here: it can only narrow what the org allows,
/// which its own typed rule does.
///
/// `None` when the input has no plugin gate. The default is fixed in code,
/// so it is not part of the PolicySetDigest.
pub fn plugins_closed_default(
    input: &Value,
    ctx: &MatchContext,
    policies: &[PolicyIr],
) -> Option<PolicyResult> {
    let listed: Vec<&str> = policies
        .iter()
        .filter(|p| p.level == Level::Org && p.mode == Mode::Enforce && matches(&p.match_, ctx))
        .flat_map(|p| &p.rules)
        .filter_map(|r| match &r.predicate {
            Predicate::Typed(TypedPredicate::PluginsAllowed { installations }) => {
                Some(installations)
            }
            _ => None,
        })
        .flatten()
        .map(String::as_str)
        .collect();
    let outcome = match units(input) {
        Err(e) => {
            // An input that cannot be read is checked by nothing else
            // either; fail closed only when it may hold a plugin gate.
            let gates = input.get("gates").and_then(Value::as_array);
            if gates.is_some_and(|g| g.is_empty()) {
                return None;
            }
            PolicyOutcome::Error {
                message: format!("the plugin default could not be evaluated, so it denies: {e}"),
            }
        }
        Ok(units) => {
            let gates: Vec<(&str, &str)> = units
                .iter()
                .filter(|u| u.what == "Gate")
                .flat_map(|u| {
                    u.origins
                        .iter()
                        .filter(|o| o.kind == "plugin")
                        .map(|o| (u.id, o.name))
                })
                .collect();
            if gates.is_empty() {
                return None;
            }
            let mut messages: Vec<String> = gates
                .iter()
                .filter(|(_, plugin)| !listed.contains(plugin))
                .map(|(id, plugin)| {
                    format!(
                        "Gate `{id}` comes from plugin `{plugin}`, which no org-level \
                         plugins_allowed setting lists; no plugin's gates run until an org \
                         admin lists the plugin."
                    )
                })
                .collect();
            messages.dedup();
            if messages.is_empty() {
                PolicyOutcome::Passed
            } else {
                PolicyOutcome::Denied { messages }
            }
        }
    };
    Some(PolicyResult {
        policy_id: PLUGINS_DEFAULT_ID.to_string(),
        version: 0,
        name: "plugins-allowed-default".to_string(),
        level: Level::Org,
        mode: Mode::Enforce,
        outcome,
    })
}

/// One origin, read from the input's JSON.
struct OriginRef<'a> {
    kind: &'a str,
    name: &'a str,
    source: Option<&'a str>,
}

impl OriginRef<'_> {
    /// What a typed-setting pattern is matched against.
    fn key(&self) -> &str {
        self.source.unwrap_or(self.name)
    }

    fn describe(&self) -> String {
        match self.kind {
            "file" => format!("file `{}`", self.name),
            "plugin" => format!("plugin `{}`", self.name),
            _ => format!("library `{}`", self.key()),
        }
    }
}

/// A Step or gate of the input with its origin chain.
struct Unit<'a> {
    /// `Step` or `Gate`, as messages name it.
    what: &'static str,
    id: &'a str,
    origins: Vec<OriginRef<'a>>,
}

/// Each Step and gate of the input.
fn units(input: &Value) -> Result<Vec<Unit<'_>>, String> {
    let mut out = Vec::new();
    for (list, what) in [("steps", "Step"), ("gates", "Gate")] {
        let items = input
            .get(list)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("the input has no `{list}` list"))?;
        for (i, item) in items.iter().enumerate() {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("`{list}[{i}]` has no id"))?;
            let origins = match item.get("origin") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(chain)) => chain
                    .iter()
                    .enumerate()
                    .map(|(j, o)| {
                        let field = |f: &str| o.get(f).and_then(Value::as_str);
                        Ok(OriginRef {
                            kind: field("kind")
                                .ok_or_else(|| format!("`{list}[{i}].origin[{j}]` has no kind"))?,
                            name: field("name")
                                .ok_or_else(|| format!("`{list}[{i}].origin[{j}]` has no name"))?,
                            source: field("source"),
                        })
                    })
                    .collect::<Result<_, String>>()?,
                Some(_) => return Err(format!("`{list}[{i}].origin` is not a list")),
            };
            out.push(Unit { what, id, origins });
        }
    }
    Ok(out)
}

/// The deny messages of a typed predicate over `input`; empty when it holds.
fn typed(t: &TypedPredicate, input: &Value) -> Result<Vec<String>, String> {
    let mut messages = Vec::new();
    for Unit { what, id, origins } in units(input)? {
        let offending = origins.iter().find(|o| match t {
            TypedPredicate::LibrariesAllowed { sources } => {
                matches!(o.kind, "library" | "file")
                    && !sources.iter().any(|p| pattern_matches(p, o.key()))
            }
            TypedPredicate::LibrariesBlocked { sources } => {
                sources.iter().any(|p| pattern_matches(p, o.key()))
            }
            TypedPredicate::PluginsAllowed { installations } => {
                what == "Gate" && o.kind == "plugin" && !installations.iter().any(|n| n == o.name)
            }
        });
        if let Some(o) = offending {
            let why = match t {
                TypedPredicate::LibrariesAllowed { .. } => {
                    "which is not in the libraries allowed here"
                }
                TypedPredicate::LibrariesBlocked { .. } => "which is blocked here",
                TypedPredicate::PluginsAllowed { .. } => {
                    "which is not in the plugins whose gates may run here"
                }
            };
            messages.push(format!("{what} `{id}` comes from {}, {why}.", o.describe()));
        }
    }
    if messages.len() > MAX_TYPED_MESSAGES {
        let more = messages.len() - MAX_TYPED_MESSAGES;
        messages.truncate(MAX_TYPED_MESSAGES);
        messages.push(format!("…and {more} more Steps or gates."));
    }
    Ok(messages)
}
