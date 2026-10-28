//! Policy IR (ADR-0075 §1): what both frontends, CEL rules and typed
//! settings, compile to, and what the evaluator runs.
//!
//! [`compile`] validates an authored [`Policy`] for one [`Level`] and
//! type-checks its CEL against the checkpoint's schema. A CEL predicate keeps
//! its checked source, not a parsed program (see [`crate::check`]), so the IR
//! is plain data: `Clone`, comparable, and serialisable.

use serde::{Deserialize, Serialize};

use crate::check::{check, Diagnostic};
use crate::policy::{Body, Checkpoint, Effect, Match, Mode, Policy, TypedSetting};
use crate::schema::{Schema, Type};

/// The longest Policy name.
pub const MAX_NAME_LEN: usize = 100;
/// The most rules one Policy may have.
pub const MAX_RULES: usize = 32;
/// The most entries in one `match` list or typed-setting list.
pub const MAX_LIST: usize = 256;
/// The longest rule message, glob or pattern.
pub const MAX_TEXT_LEN: usize = 1024;
/// The most distinct `matches()` patterns one Policy may use across its
/// rules; one rule uses at most [`MAX_PATTERNS`](crate::check::MAX_PATTERNS).
pub const MAX_POLICY_PATTERNS: usize = 32;

/// Where a Policy attaches (ADR-0075 §7). The Environment layer is not built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Org,
    Project,
}

/// A CEL `when`, type-checked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CelPredicate {
    pub source: String,
    pub cost: u64,
    pub warnings: Vec<Diagnostic>,
    /// See [`Checked::reads`](crate::check::Checked::reads).
    pub reads: Vec<String>,
}

/// A typed setting's predicate: true when the input breaks the setting. See
/// [`TypedSetting`] for each meaning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypedPredicate {
    LibrariesAllowed { sources: Vec<String> },
    LibrariesBlocked { sources: Vec<String> },
    PluginsAllowed { installations: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    Cel(CelPredicate),
    Typed(TypedPredicate),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuleIr {
    pub predicate: Predicate,
    pub effect: Effect,
    /// The rule's message. A typed predicate writes its own messages, naming
    /// the offending Step or gate; this one is its fallback.
    pub message: String,
}

/// One compiled Policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyIr {
    /// The stored Policy's id; empty for a draft (the policy test API).
    pub policy_id: String,
    /// The stored PolicyVersion; 0 for a draft.
    pub version: u64,
    /// [`Policy::content_hash`] of the authored Policy.
    pub content_hash: String,
    pub name: String,
    pub level: Level,
    pub checkpoint: Checkpoint,
    #[serde(rename = "match")]
    pub match_: Match,
    pub mode: Mode,
    pub rules: Vec<RuleIr>,
}

impl PolicyIr {
    /// The same IR, naming the stored Policy and version it was compiled
    /// from.
    pub fn with_identity(mut self, policy_id: impl Into<String>, version: u64) -> PolicyIr {
        self.policy_id = policy_id.into();
        self.version = version;
        self
    }

    /// The checker's warnings over every CEL rule (`has()` guards).
    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.rules.iter().flat_map(|r| match &r.predicate {
            Predicate::Cel(c) => c.warnings.as_slice(),
            Predicate::Typed(_) => &[],
        })
    }
}

/// The input schema of `checkpoint` at `version`, when the engine has one.
pub fn schema_for(checkpoint: Checkpoint, version: u32) -> Option<Schema> {
    match checkpoint {
        Checkpoint::RunCreate if version == crate::run_create::SCHEMA_VERSION => {
            Some(crate::run_create::schema())
        }
        _ => None,
    }
}

/// The current input schema version of `checkpoint`, when it is consulted.
pub fn schema_version(checkpoint: Checkpoint) -> Option<u32> {
    match checkpoint {
        Checkpoint::RunCreate => Some(crate::run_create::SCHEMA_VERSION),
        _ => None,
    }
}

/// Validates `policy` for `level` and compiles it. Every finding is
/// returned, not only the first; a CEL rule contributes its checker's
/// diagnostics, each with `at` naming the rule.
///
/// The IR names no stored Policy yet; see [`PolicyIr::with_identity`].
pub fn compile(policy: &Policy, level: Level) -> Result<PolicyIr, Vec<Diagnostic>> {
    let mut errors = Vec::new();
    validate_name(&policy.name, &mut errors);
    validate_match(&policy.match_, &mut errors);

    if !policy.checkpoint.is_consulted() {
        errors.push(Diagnostic::at(
            "checkpoint",
            format!(
                "checkpoint `{}` is not yet supported; only `run.create` is consulted",
                policy.checkpoint
            ),
        ));
        return Err(errors);
    }
    let version = schema_version(policy.checkpoint).expect("a consulted checkpoint has a schema");
    let schema = schema_for(policy.checkpoint, version).expect("the current schema exists");

    let rules = match &policy.body {
        Body::Rules { .. } if level == Level::Project => {
            errors.push(Diagnostic::at(
                "rules",
                "Project admins write typed settings only (ADR-0075 §7); \
                 use `setting`, or write CEL rules at the org level",
            ));
            Vec::new()
        }
        Body::Rules { rules } => {
            if rules.is_empty() {
                errors.push(Diagnostic::at("rules", "a Policy needs at least one rule"));
            }
            if rules.len() > MAX_RULES {
                errors.push(Diagnostic::at(
                    "rules",
                    format!(
                        "a Policy has at most {MAX_RULES} rules; this one has {}",
                        rules.len()
                    ),
                ));
            }
            let mut out = Vec::new();
            let mut patterns = std::collections::BTreeSet::new();
            for (i, rule) in rules.iter().enumerate() {
                let at = format!("rules[{i}]");
                if !policy.checkpoint.allows(&rule.effect) {
                    errors.push(Diagnostic::at(
                        format!("{at}.effect"),
                        format!(
                            "`{}` is not allowed at `{}`, which allows `deny` only",
                            rule.effect.name(),
                            policy.checkpoint
                        ),
                    ));
                }
                validate_text(
                    &rule.message,
                    &format!("{at}.message"),
                    "a message",
                    &mut errors,
                );
                match check(&schema, &rule.when, &Type::Bool) {
                    Ok(checked) => {
                        patterns.extend(checked.patterns);
                        out.push(RuleIr {
                            predicate: Predicate::Cel(CelPredicate {
                                source: checked.source,
                                cost: checked.cost,
                                warnings: checked
                                    .warnings
                                    .into_iter()
                                    .map(|w| located(w, &format!("{at}.when")))
                                    .collect(),
                                reads: checked.reads.into_iter().collect(),
                            }),
                            effect: rule.effect.clone(),
                            message: rule.message.clone(),
                        })
                    }
                    Err(ds) => {
                        errors.extend(ds.into_iter().map(|d| located(d, &format!("{at}.when"))))
                    }
                }
            }
            if patterns.len() > MAX_POLICY_PATTERNS {
                errors.push(Diagnostic::at(
                    "rules",
                    format!(
                        "a Policy uses at most {MAX_POLICY_PATTERNS} distinct matches() \
                         patterns; this one uses {}",
                        patterns.len()
                    ),
                ));
            }
            out
        }
        Body::Setting(setting) => {
            if policy.checkpoint != Checkpoint::RunCreate {
                errors.push(Diagnostic::at(
                    "checkpoint",
                    format!(
                        "`{}` compiles to `run.create`, not `{}`",
                        setting.name(),
                        policy.checkpoint
                    ),
                ));
            }
            vec![compile_setting(setting, &mut errors)]
        }
    };

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(PolicyIr {
        policy_id: String::new(),
        version: 0,
        content_hash: policy.content_hash(),
        name: policy.name.clone(),
        level,
        checkpoint: policy.checkpoint,
        match_: policy.match_.clone(),
        mode: policy.mode,
        rules,
    })
}

fn located(mut d: Diagnostic, at: &str) -> Diagnostic {
    d.at = at.to_string();
    d
}

fn compile_setting(setting: &TypedSetting, errors: &mut Vec<Diagnostic>) -> RuleIr {
    let (field, list) = match setting {
        TypedSetting::LibrariesAllowed { sources } | TypedSetting::LibrariesBlocked { sources } => {
            ("sources", sources)
        }
        TypedSetting::PluginsAllowed { installations } => ("installations", installations),
    };
    let at = format!("setting.{}.{field}", setting.name());
    validate_list(list, &at, errors);
    for (i, p) in list.iter().enumerate() {
        let wildcard = p.find('*');
        if wildcard.is_some_and(|w| w != p.len() - 1)
            || (field == "installations" && wildcard.is_some())
        {
            errors.push(Diagnostic::at(
                format!("{at}[{i}]"),
                if field == "installations" {
                    format!("`{p}`: an installation is named exactly, with no `*`")
                } else {
                    format!("`{p}`: a pattern is an exact name or a prefix ending in `*`")
                },
            ));
        }
    }
    let (predicate, message) = match setting.clone() {
        TypedSetting::LibrariesAllowed { sources } => (
            TypedPredicate::LibrariesAllowed { sources },
            "This Run uses a library that is not allowed here.",
        ),
        TypedSetting::LibrariesBlocked { sources } => (
            TypedPredicate::LibrariesBlocked { sources },
            "This Run uses a blocked library.",
        ),
        TypedSetting::PluginsAllowed { installations } => (
            TypedPredicate::PluginsAllowed { installations },
            "This Run has a gate from a plugin that is not allowed here.",
        ),
    };
    RuleIr {
        predicate: Predicate::Typed(predicate),
        effect: Effect::Deny,
        message: message.to_string(),
    }
}

fn validate_name(name: &str, errors: &mut Vec<Diagnostic>) {
    let ok_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if name.is_empty() || name.len() > MAX_NAME_LEN || !name.chars().all(ok_char) {
        errors.push(Diagnostic::at(
            "name",
            format!(
                "`{name}`: a Policy name is 1 to {MAX_NAME_LEN} letters, digits, `-`, `_` or `.`"
            ),
        ));
    }
}

fn validate_match(m: &Match, errors: &mut Vec<Diagnostic>) {
    validate_list(&m.projects, "match.projects", errors);
    validate_list(&m.except_projects, "match.except_projects", errors);
    validate_list(&m.environments, "match.environments", errors);
    if m.trigger_kinds.len() > MAX_LIST {
        errors.push(Diagnostic::at(
            "match.trigger_kinds",
            format!("at most {MAX_LIST} entries"),
        ));
    }
}

fn validate_list(list: &[String], at: &str, errors: &mut Vec<Diagnostic>) {
    if list.len() > MAX_LIST {
        errors.push(Diagnostic::at(
            at,
            format!("at most {MAX_LIST} entries; this has {}", list.len()),
        ));
    }
    for (i, item) in list.iter().enumerate() {
        validate_text(item, &format!("{at}[{i}]"), "an entry", errors);
    }
}

fn validate_text(text: &str, at: &str, what: &str, errors: &mut Vec<Diagnostic>) {
    if text.trim().is_empty() {
        errors.push(Diagnostic::at(at, format!("{what} cannot be empty")));
    } else if text.len() > MAX_TEXT_LEN {
        errors.push(Diagnostic::at(
            at,
            format!("{what} is at most {MAX_TEXT_LEN} bytes"),
        ));
    }
}
