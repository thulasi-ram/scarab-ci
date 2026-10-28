//! Evaluating a checked rule with `cel` 0.15, failing closed.
//!
//! The input is the checkpoint's Rust value, serialised and converted to CEL
//! values under the same schema the rule was checked against: structs become
//! maps, an absent optional field is a missing key (so `has()` sees it), an
//! absent optional variable is `null`, and a [`Timestamp`](crate::schema::Timestamp)
//! becomes a CEL `timestamp`.

use std::collections::HashMap;
use std::sync::Arc;

use cel::objects::{Key, Map};
use cel::{Context, Value};

use crate::check::{check, Checked, Diagnostic};
use crate::schema::{Schema, Type};

/// One rule of a Policy, checked.
#[derive(Debug)]
pub struct Rule {
    pub when: Checked,
    pub message: String,
    pub message_expr: Option<Checked>,
}

impl Rule {
    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.when
            .warnings
            .iter()
            .chain(self.message_expr.iter().flat_map(|m| &m.warnings))
    }
}

/// Checks a rule's `when` (must be bool) and `message_expr` (must be string).
pub fn compile_rule(
    schema: &Schema,
    when: &str,
    message: &str,
    message_expr: Option<&str>,
) -> Result<Rule, Vec<Diagnostic>> {
    let when = check(schema, when, &Type::Bool)?;
    let message_expr = message_expr
        .map(|m| check(schema, m, &Type::String))
        .transpose()?;
    Ok(Rule {
        when,
        message: message.to_string(),
        message_expr,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuleOutcome {
    /// `when` was false: the rule's effect does not apply.
    DoesNotApply,
    /// `when` was true: the rule's effect applies, with this message.
    Applies { message: String },
    /// Evaluation failed. Whatever the rule's effect, the decision is deny.
    DeniedOnError { message: String, error: String },
}

/// Evaluates `rule` against `input`. Any error, in binding the input or in
/// evaluating `when`, denies (ADR-0075 §6).
pub fn evaluate_rule<I: serde::Serialize>(schema: &Schema, rule: &Rule, input: &I) -> RuleOutcome {
    let denied = |error: String| RuleOutcome::DeniedOnError {
        message: rule.message.clone(),
        error,
    };
    let context = match bind(schema, input) {
        Ok(c) => c,
        Err(e) => return denied(format!("input does not match the schema: {e}")),
    };
    match rule.when.program.execute(&context) {
        Ok(Value::Bool(false)) => RuleOutcome::DoesNotApply,
        Ok(Value::Bool(true)) => {
            let message = match &rule.message_expr {
                Some(m) => match m.program.execute(&context) {
                    Ok(Value::String(s)) => s.to_string(),
                    // The `when` held, so the effect applies either way; a
                    // broken message only loses its detail.
                    _ => rule.message.clone(),
                },
                None => rule.message.clone(),
            };
            RuleOutcome::Applies { message }
        }
        Ok(other) => denied(format!("`when` returned {other:?}, not a bool")),
        Err(e) => denied(e.to_string()),
    }
}

/// Binds each top-level field of `input` as a CEL variable.
pub fn bind<I: serde::Serialize>(
    schema: &Schema,
    input: &I,
) -> Result<Context<'static, 'static>, String> {
    let json = serde_json::to_value(input).map_err(|e| e.to_string())?;
    let serde_json::Value::Object(obj) = json else {
        return Err("the input is not a struct".to_string());
    };
    let mut context = Context::default();
    for var in &schema.variables {
        let value = match obj.get(&var.name) {
            None | Some(serde_json::Value::Null) if var.optional => Value::Null,
            None | Some(serde_json::Value::Null) => {
                return Err(format!("`{}` is required", var.name));
            }
            Some(v) => to_value(schema, &var.ty, v).map_err(|e| format!("{}{e}", var.name))?,
        };
        context.add_variable_from_value(var.name.clone(), value);
    }
    Ok(context)
}

/// Converts `json` to a CEL value of type `ty`. Errors carry the path below
/// the value, e.g. `.target.kind: ...`.
fn to_value(schema: &Schema, ty: &Type, json: &serde_json::Value) -> Result<Value, String> {
    use serde_json::Value as J;
    Ok(match (ty, json) {
        (Type::Bool, J::Bool(b)) => Value::Bool(*b),
        (Type::String, J::String(s)) => Value::String(Arc::new(s.clone())),
        (Type::Int, J::Number(n)) if n.is_i64() => Value::Int(n.as_i64().unwrap_or_default()),
        (Type::Uint, J::Number(n)) if n.is_u64() => Value::UInt(n.as_u64().unwrap_or_default()),
        (Type::Double, J::Number(n)) => Value::Float(n.as_f64().unwrap_or_default()),
        (Type::Timestamp, J::String(s)) => {
            Value::Timestamp(chrono::DateTime::parse_from_rfc3339(s).map_err(|e| format!(": {e}"))?)
        }
        (Type::Enum(name), J::String(s)) => {
            if !schema.enum_decl(name).variants.contains(s) {
                return Err(format!(": \"{s}\" is not a {name}"));
            }
            Value::String(Arc::new(s.clone()))
        }
        (Type::List { elem, cap }, J::Array(items)) => {
            if items.len() as u64 > *cap {
                return Err(format!(": {} items, above the cap of {cap}", items.len()));
            }
            let items = items
                .iter()
                .enumerate()
                .map(|(i, v)| to_value(schema, elem, v).map_err(|e| format!("[{i}]{e}")))
                .collect::<Result<Vec<_>, _>>()?;
            Value::List(Arc::new(items))
        }
        (Type::Struct(name), J::Object(obj)) => {
            let decl = schema.struct_decl(name);
            let mut map = HashMap::new();
            for field in &decl.fields {
                match obj.get(&field.name) {
                    None | Some(J::Null) if field.optional => {}
                    None | Some(J::Null) => return Err(format!(".{}: required", field.name)),
                    Some(v) => {
                        let v = to_value(schema, &field.ty, v)
                            .map_err(|e| format!(".{}{e}", field.name))?;
                        map.insert(Key::String(Arc::new(field.name.clone())), v);
                    }
                }
            }
            Value::Map(Map { map: Arc::new(map) })
        }
        (ty, v) => return Err(format!(": expected {ty}, got {v}")),
    })
}
