//! The authored Policy (ADR-0075 §4, §12): what an admin writes and Scarab
//! stores, before it is compiled to [`PolicyIr`](crate::ir::PolicyIr).
//!
//! A Policy names one [`Checkpoint`], a typed [`Match`], a [`Mode`] and a
//! [`Body`]: either CEL rules or one [`TypedSetting`]. On the wire the body is
//! one of two keys, `rules` or `setting`:
//!
//! ```json
//! { "name": "payments-prod-libraries", "checkpoint": "run.create",
//!   "match": { "projects": ["payments-*"], "environments": ["production"] },
//!   "mode": "enforce",
//!   "rules": [{ "when": "steps.exists(s, s.deploy)", "effect": "deny",
//!               "message": "No deploys." }] }
//!
//! { "name": "libraries", "checkpoint": "run.create",
//!   "setting": { "libraries_allowed": { "sources": ["github.com/acme/*"] } } }
//! ```
//!
//! Unknown fields are refused everywhere, so a misspelt key is an error when
//! the Policy is saved rather than a rule that silently matches nothing.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::run_create::TriggerKind;

/// A place where Scarab consults the engine (ADR-0075 §2). The set is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Checkpoint {
    #[serde(rename = "run.create")]
    RunCreate,
    #[serde(rename = "step.admit")]
    StepAdmit,
    #[serde(rename = "gate.resolve")]
    GateResolve,
}

impl Checkpoint {
    pub const ALL: [Checkpoint; 3] = [
        Checkpoint::RunCreate,
        Checkpoint::StepAdmit,
        Checkpoint::GateResolve,
    ];

    /// The wire name, e.g. `run.create`.
    pub fn as_str(self) -> &'static str {
        match self {
            Checkpoint::RunCreate => "run.create",
            Checkpoint::StepAdmit => "step.admit",
            Checkpoint::GateResolve => "gate.resolve",
        }
    }

    pub fn parse(s: &str) -> Option<Checkpoint> {
        Checkpoint::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// Whether the engine consults this checkpoint yet. Only `run.create` is
    /// built; a Policy naming another is refused when saved.
    pub fn is_consulted(self) -> bool {
        self == Checkpoint::RunCreate
    }

    /// Whether `effect` is allowed here (ADR-0075 §5): `run.create` and
    /// `step.admit` deny only; `gate.resolve` takes all three.
    pub fn allows(self, effect: &Effect) -> bool {
        matches!(effect, Effect::Deny) || self == Checkpoint::GateResolve
    }
}

impl fmt::Display for Checkpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a Policy's denies are enforced, or only recorded as "would deny".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Enforce,
    Audit,
}

/// The typed selector evaluated before any rule (ADR-0075 §4). An empty list
/// matches anything. Globs have one wildcard, `*`, matching any run of
/// characters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    #[serde(default)]
    pub projects: Vec<String>,
    /// Projects the Policy does not apply to, even when `projects` matches:
    /// the per-repository allow of ADR-0075 §7.
    #[serde(default)]
    pub except_projects: Vec<String>,
    /// Environment names. A Run with no Environment matches only an empty
    /// list.
    #[serde(default)]
    pub environments: Vec<String>,
    #[serde(default)]
    pub trigger_kinds: Vec<TriggerKind>,
}

/// What a matched rule does (ADR-0075 §5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Effect {
    Deny,
    RequireApproval { principals: Vec<String>, count: u32 },
    RequireStepUp,
}

impl Effect {
    pub fn name(&self) -> &'static str {
        match self {
            Effect::Deny => "deny",
            Effect::RequireApproval { .. } => "require_approval",
            Effect::RequireStepUp => "require_step_up",
        }
    }
}

/// One CEL rule: `when` over the checkpoint's input, an effect and a static
/// message (ADR-0075 §4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub when: String,
    pub effect: Effect,
    pub message: String,
}

/// A typed setting (ADR-0075 §12, ADR-0076): a form-editable rule that
/// compiles to a typed predicate, not to CEL text.
///
/// Patterns are an exact name or a prefix ending in `*`, matched against an
/// origin's `source`, or its `name` when it has no source (a `file` origin's
/// name is its repository path).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TypedSetting {
    /// "Libraries allowed here": deny a Run in which a Step or gate comes
    /// from a `library` or `file` origin matching none of `sources`.
    LibrariesAllowed { sources: Vec<String> },
    /// Deny a Run in which a Step or gate comes from an origin matching any
    /// of `sources`.
    LibrariesBlocked { sources: Vec<String> },
    /// "Plugins whose gates may run here": deny a Run with a gate whose
    /// `plugin` origin names an installation not in `installations`.
    PluginsAllowed { installations: Vec<String> },
}

impl TypedSetting {
    pub fn name(&self) -> &'static str {
        match self {
            TypedSetting::LibrariesAllowed { .. } => "libraries_allowed",
            TypedSetting::LibrariesBlocked { .. } => "libraries_blocked",
            TypedSetting::PluginsAllowed { .. } => "plugins_allowed",
        }
    }
}

/// A Policy's body: CEL rules, or one typed setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    Rules { rules: Vec<Rule> },
    Setting(TypedSetting),
}

/// An authored Policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PolicyWire", into = "PolicyWire")]
pub struct Policy {
    pub name: String,
    pub checkpoint: Checkpoint,
    pub match_: Match,
    pub mode: Mode,
    pub body: Body,
}

/// The wire shape of a [`Policy`]: the body is `rules` or `setting`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyWire {
    /// Accepted (and dropped) so a file can say `kind: policy`, as the ADR's
    /// examples do; any other value is refused.
    #[serde(default, skip_serializing)]
    kind: Option<String>,
    name: String,
    checkpoint: Checkpoint,
    #[serde(rename = "match", default)]
    match_: Match,
    #[serde(default)]
    mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rules: Option<Vec<Rule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    setting: Option<TypedSetting>,
}

impl TryFrom<PolicyWire> for Policy {
    type Error = String;

    fn try_from(w: PolicyWire) -> Result<Policy, String> {
        if let Some(kind) = w.kind.filter(|k| k != "policy") {
            return Err(format!("`kind` is `{kind}`; a Policy's kind is `policy`"));
        }
        let body = match (w.rules, w.setting) {
            (Some(rules), None) => Body::Rules { rules },
            (None, Some(setting)) => Body::Setting(setting),
            (Some(_), Some(_)) => {
                return Err("a Policy has `rules` or a `setting`, not both".to_string())
            }
            (None, None) => return Err("a Policy needs `rules` or a `setting`".to_string()),
        };
        Ok(Policy {
            name: w.name,
            checkpoint: w.checkpoint,
            match_: w.match_,
            mode: w.mode,
            body,
        })
    }
}

impl From<Policy> for PolicyWire {
    fn from(p: Policy) -> PolicyWire {
        let (rules, setting) = match p.body {
            Body::Rules { rules } => (Some(rules), None),
            Body::Setting(s) => (None, Some(s)),
        };
        PolicyWire {
            kind: None,
            name: p.name,
            checkpoint: p.checkpoint,
            match_: p.match_,
            mode: p.mode,
            rules,
            setting,
        }
    }
}

impl Policy {
    /// Parses a Policy from JSON, with serde's message on failure.
    pub fn from_json(value: serde_json::Value) -> Result<Policy, String> {
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    /// The Policy's canonical JSON: object keys sorted at every level, no
    /// whitespace. Two Policies that compare equal have the same canonical
    /// JSON.
    pub fn canonical_json(&self) -> String {
        let value = serde_json::to_value(self).expect("a Policy serialises");
        canonical_json(&value)
    }

    /// SHA-256 of [`canonical_json`](Self::canonical_json), lowercase hex
    /// (ADR-0075 §11).
    pub fn content_hash(&self) -> String {
        sha256_hex(self.canonical_json().as_bytes())
    }

    pub fn is_typed_setting(&self) -> bool {
        matches!(self.body, Body::Setting(_))
    }
}

/// JSON with object keys sorted at every level, independent of whether
/// `serde_json`'s `preserve_order` feature is on anywhere in the build.
pub fn canonical_json(value: &serde_json::Value) -> String {
    use serde_json::Value as J;
    fn write(v: &J, out: &mut String) {
        match v {
            J::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&J::String(k.clone()).to_string());
                    out.push(':');
                    write(&map[k], out);
                }
                out.push('}');
            }
            J::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            scalar => out.push_str(&scalar.to_string()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Whether `glob` matches `value`. `*` matches any run of characters, the
/// empty run included; every other character matches itself.
pub fn glob_matches(glob: &str, value: &str) -> bool {
    let (g, v): (Vec<char>, Vec<char>) = (glob.chars().collect(), value.chars().collect());
    let (mut gi, mut vi) = (0, 0);
    // The last `*` seen and the value position it was tried at.
    let mut star: Option<(usize, usize)> = None;
    while vi < v.len() {
        if gi < g.len() && g[gi] == '*' {
            star = Some((gi, vi));
            gi += 1;
        } else if gi < g.len() && g[gi] == v[vi] {
            gi += 1;
            vi += 1;
        } else if let Some((sg, sv)) = star {
            gi = sg + 1;
            vi = sv + 1;
            star = Some((sg, sv + 1));
        } else {
            return false;
        }
    }
    g[gi..].iter().all(|c| *c == '*')
}

/// Whether a typed-setting `pattern` matches `value`: equal, or a prefix
/// match when the pattern ends in `*`.
pub fn pattern_matches(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => pattern == value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_matches("payments-*", "payments-api"));
        assert!(glob_matches("payments-*", "payments-"));
        assert!(!glob_matches("payments-*", "payment"));
        assert!(glob_matches("*", ""));
        assert!(glob_matches("a*b*c", "aXXbYYc"));
        assert!(!glob_matches("a*b*c", "aXXbYY"));
        assert!(glob_matches("*-prod", "eu-west-prod"));
        assert!(glob_matches("exact", "exact"));
        assert!(!glob_matches("exact", "exactly"));
    }

    #[test]
    fn patterns() {
        assert!(pattern_matches(
            "github.com/acme/*",
            "github.com/acme/libs//x.yaml"
        ));
        assert!(!pattern_matches(
            "github.com/acme/*",
            "github.com/acmex/libs"
        ));
        assert!(pattern_matches("github.com/acme/x", "github.com/acme/x"));
        assert!(!pattern_matches("github.com/acme/x", "github.com/acme/xy"));
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let v = serde_json::json!({"b": 1, "a": {"d": [1, {"z": 0, "y": 1}], "c": "x"}});
        assert_eq!(
            canonical_json(&v),
            r#"{"a":{"c":"x","d":[1,{"y":1,"z":0}]},"b":1}"#
        );
    }
}
